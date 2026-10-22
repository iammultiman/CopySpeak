// TTS synthesis commands — speak_now, speak_queued, speak_history_entry.

use crate::audio::{self, AudioPlayer};
use crate::config::{self, AppConfig};
use crate::fragment_queue::FragmentQueue;
use crate::history::{self, HistoryLog};
use crate::hud;
use crate::pagination;
use crate::telemetry;
use crate::tts::captions::{read_sidecar, write_sidecar, CaptionAlignment, SpeechAudio};
use crate::tts::stream::{pcm_to_wav, AudioFormatMeta, ChunkItem, ChunkStream};
use crate::tts::TtsBackend;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, State};

use super::helpers::{
    active_engine, create_backend, create_backend_from_effective, engine_identifier, engine_str,
    resolve_effective, voice_display_name, SynthesisGuard,
};
use crate::commands::{AudioFragmentEvent, AudioStreamChunkEvent, CachedAudio, PaginationEvent};

// ── Helper Functions ──────────────────────────────────────────────────────────

/// Truncate text for preview display
fn truncate_preview(text: &str, max_len: usize) -> String {
    if text.len() <= max_len {
        text.to_string()
    } else {
        format!("{}...", text.chars().take(max_len).collect::<String>())
    }
}

/// Get text from parameter or clipboard
fn get_text_or_clipboard(text: Option<String>) -> Result<String, String> {
    match text {
        Some(t) => Ok(t),
        None => {
            crate::clipboard::get_clipboard_text().ok_or_else(|| "No text in clipboard".to_string())
        }
    }
}

/// Whether a new reading request must abort the one already in progress.
///
/// `on_retrigger` is enforced here rather than in the native Rodio player: the
/// webview owns playback for every reading, so only this path can stop the
/// audio the user actually hears.
fn retrigger_interrupts(mode: &config::RetriggerMode) -> bool {
    matches!(mode, config::RetriggerMode::Interrupt)
}

/// Whether a finished reading may publish its result — the Replay cache and the
/// terminal `pagination:complete` event.
///
/// A reading that is no longer current must stay quiet. Its cached audio is a
/// reading the user cancelled, and its completion event would tell the frontend
/// the *replacement* has ended, finishing the replacement's still-empty queue
/// before it has played anything.
fn reading_may_publish(own_generation: u32, current_generation: u32) -> bool {
    own_generation == current_generation
}

/// Log debug info for TTS operations
fn log_tts_debug(tag: &str, backend: &str, text: &str) {
    if crate::logging::is_debug_mode() {
        let text_preview: String = text.chars().take(100).collect();
        log::debug!("[{}] called (Backend: {:?})", tag, backend);
        log::debug!("[{}] Text length: {} chars", tag, text.len());
        log::debug!(
            "[{}] Text preview: {:?}{}",
            tag,
            text_preview,
            if text.len() > 100 { "..." } else { "" }
        );
    }
}

/// Synthesize audio using spawn_blocking
async fn synthesize_async(
    backend: Arc<Box<dyn TtsBackend>>,
    text: String,
    voice: String,
) -> Result<SpeechAudio, String> {
    let generation = crate::ABORT_GENERATION.load(Ordering::SeqCst);
    let cancelled = || crate::ABORT_REQUESTED.load(Ordering::Relaxed)
        || generation != crate::ABORT_GENERATION.load(Ordering::SeqCst);
    let result = retry_synthesis(
        || {
            let backend = backend.clone();
            let text = text.clone();
            let voice = voice.clone();
            async move {
                tokio::task::spawn_blocking(move || backend.synthesize_with_captions(&text, &voice))
                    .await
                    .map_err(|e| format!("Task join error: {e}"))?
                    .map_err(|e| e.to_string())
            }
        },
        cancelled,
        // A backend that just told us the provider refused this request keeps
        // that verdict: retrying amplifies throttling rather than recovering.
        || !backend.failure_is_final(),
        Duration::from_millis(500),
    ).await;
    if cancelled() { Ok(Vec::new().into()) } else { result }
}

/// Wording for a section that could not be synthesized.
///
/// Pure so it can be asserted directly. The reading loop returns after this
/// error, so this section and every later one are never spoken; the earlier
/// wording ("No text was skipped") only described the *already synthesized*
/// fragments and read as though the whole passage had been read.
fn section_failure_message(index: usize, total: usize, error: &str) -> String {
    if total <= 1 {
        format!("Reading failed: {error}")
    } else if index.saturating_add(1) >= total {
        // The final section failed, so nothing follows it; claiming "the rest
        // of the passage was not read" would describe text that does not exist.
        // `saturating_add` keeps a defensive index from panicking in here.
        format!(
            "Reading stopped: the final section ({} of {}) failed. {error}",
            index.saturating_add(1),
            total
        )
    } else {
        format!(
            "Reading stopped: section {} of {} failed, so the rest of the passage was not read. {error}",
            index + 1,
            total
        )
    }
}

fn report_section_failure(
    app: &AppHandle,
    index: usize,
    total: usize,
    generation: u32,
    error: String,
) -> String {
    let message = section_failure_message(index, total, &error);
    if reading_may_publish(generation, crate::ABORT_GENERATION.load(Ordering::SeqCst))
        && !crate::ABORT_REQUESTED.load(Ordering::Relaxed)
    {
        log::error!("[TTS] {message}");
        let _ = app.emit("reading-failed", &message);
    }
    message
}

fn transient_synthesis_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    // Every condition is evaluated; a permanent code must not short-circuit the
    // scan. edge-tts reports its real failure as
    // `NoAudioReceived("No audio was received...")` next to the `403` from the
    // refused handshake. The previous first-token-wins form read that `403`,
    // returned "permanent", and never reached the phrase list below — so the
    // transient no-audio failure was never retried.
    let retryable_status = (message.contains("http")
        || message.contains("status")
        || message.contains("api error"))
        && message
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|token| matches!(token, "429" | "500" | "502" | "503" | "504"));
    if retryable_status {
        return true;
    }
    ["timeout", "timed out",
        "connection reset", "connection closed", "connection aborted",
        "connection refused", "no audio was received", "daemon closed its output",
        // reqwest's own transport failures. Its `Display` prints only the error
        // kind ("error sending request"), so a timeout or a cut connection is
        // recognised by the kind even when the reason chain is unavailable.
        "error sending request", "request or response body error"]
        .iter()
        .any(|reason| message.contains(reason))
}

async fn retry_synthesis<T, Operation, Attempt, Cancelled, CanRetry>(
    mut operation: Operation,
    is_cancelled: Cancelled,
    can_retry: CanRetry,
    retry_delay: Duration,
) -> Result<T, String>
where
    Operation: FnMut() -> Attempt,
    Attempt: std::future::Future<Output = Result<T, String>>,
    Cancelled: Fn() -> bool,
    CanRetry: Fn() -> bool,
{
    for attempt in 0..3 {
        if is_cancelled() {
            return Err("Reading cancelled".into());
        }
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if is_cancelled() {
                    return Err("Reading cancelled".into());
                }
                if attempt == 2 || !can_retry() || !transient_synthesis_error(&error) {
                    return Err(error);
                }
                log::warn!("[TTS] Transient synthesis failure; retry {}/2: {}", attempt + 1, error);
                let deadline = tokio::time::Instant::now() + retry_delay * (attempt + 1);
                while tokio::time::Instant::now() < deadline {
                    if is_cancelled() {
                        return Err("Reading cancelled".into());
                    }
                    tokio::time::sleep(Duration::from_millis(50).min(
                        deadline.saturating_duration_since(tokio::time::Instant::now()),
                    )).await;
                }
            }
        }
    }
    unreachable!()
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};

    #[tokio::test]
    async fn retries_transient_errors_without_skipping_text() {
        let attempts = AtomicUsize::new(0);
        let result = retry_synthesis(
            || std::future::ready(if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                Err("HTTP error: 503 Service Unavailable".into())
            } else { Ok("complete section") }),
            || false, || true, Duration::ZERO,
        ).await;
        assert_eq!(result.unwrap(), "complete section");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn permanent_and_partially_played_failures_are_not_retried() {
        for (error, can_retry) in [("401 Unauthorized", true), ("HTTP 503", false)] {
            let attempts = AtomicUsize::new(0);
            let result: Result<(), String> = retry_synthesis(
                || { attempts.fetch_add(1, Ordering::SeqCst); std::future::ready(Err(error.into())) },
                || false, || can_retry, Duration::ZERO,
            ).await;
            assert_eq!(result.unwrap_err(), error);
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn retries_are_bounded_and_cancellation_prevents_another_attempt() {
        let attempts = AtomicUsize::new(0);
        let result: Result<(), String> = retry_synthesis(
            || { attempts.fetch_add(1, Ordering::SeqCst); std::future::ready(Err("HTTP 429".into())) },
            || false, || true, Duration::ZERO,
        ).await;
        assert_eq!(result.unwrap_err(), "HTTP 429");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        let cancelled = AtomicBool::new(false);
        let result: Result<(), String> = retry_synthesis(
            || { cancelled.store(true, Ordering::SeqCst); std::future::ready(Err("HTTP 503".into())) },
            || cancelled.load(Ordering::SeqCst), || true, Duration::ZERO,
        ).await;
        assert_eq!(result.unwrap_err(), "Reading cancelled");
    }

    #[tokio::test]
    async fn cancellation_interrupts_backoff_before_another_request() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = cancelled.clone();
        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            signal.store(true, Ordering::SeqCst);
        });
        let attempts = AtomicUsize::new(0);
        let result: Result<(), String> = tokio::time::timeout(Duration::from_secs(1), retry_synthesis(
            || { attempts.fetch_add(1, Ordering::SeqCst); std::future::ready(Err("HTTP 503".into())) },
            || cancelled.load(Ordering::SeqCst), || true, Duration::from_secs(60),
        )).await.unwrap();
        cancel_task.await.unwrap();
        assert_eq!(result.unwrap_err(), "Reading cancelled");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn status_classification_does_not_retry_permanent_or_incidental_numbers() {
        // A permanent HTTP status is not retried on its own.
        assert!(!transient_synthesis_error("HTTP 401 Unauthorized: invalid API key"));
        assert!(!transient_synthesis_error("HTTP 403: voice model not found"));
        assert!(!transient_synthesis_error("voice model 50300 was not found"));
        assert!(!transient_synthesis_error("invalid transcript contains 503"));
        assert!(transient_synthesis_error("HTTP error: Google API error 503 Service Unavailable"));
        assert!(transient_synthesis_error("WSServerHandshakeError: 503 Invalid response status"));
    }

    #[test]
    fn transient_signal_beats_a_coincidental_permanent_status() {
        // The exact shape of the user's Edge failure: the refused handshake's
        // 403 precedes the real no-audio error, and must not mask it.
        assert!(transient_synthesis_error(
            "edge-tts exit exit status: 1: NoAudioReceived(\"No audio was received. \
             Please verify that your parameters are correct.\") status=403"
        ));
        // A retryable status counts wherever it appears in the string.
        assert!(transient_synthesis_error("status 403 handled; upstream returned 503"));
        // A provider status error need not say "http" or "status".
        assert!(transient_synthesis_error(
            "ElevenLabs API error 503 Service Unavailable: overloaded"
        ));
        // reqwest reports transport failures as a bare error kind.
        assert!(transient_synthesis_error("Request failed: error sending request"));
        assert!(transient_synthesis_error(
            "Streaming synthesis failed mid-stream: request or response body error"
        ));
        assert!(transient_synthesis_error(
            "Request failed: error sending request: operation timed out"
        ));
        // A response-body read failure keeps its cause in the flattened chain.
        assert!(transient_synthesis_error(
            "Failed to read bytes: request or response body error: connection reset by peer"
        ));
        // A local CLI `exit status: 1` is not itself a transient signal.
        assert!(!transient_synthesis_error(
            "edge-tts exit exit status: 1: ERROR: voice model not found"
        ));
    }

    #[test]
    fn failure_message_states_that_the_rest_was_not_read() {
        let many = section_failure_message(3, 10, "HTTP 503 Service Unavailable");
        assert!(many.contains("section 4 of 10"));
        assert!(many.contains("the rest of the passage was not read"));
        assert!(!many.contains("No text was skipped"));
        // A single-section reading has no "section 1 of 1" to report.
        assert_eq!(
            section_failure_message(0, 1, "HTTP 503"),
            "Reading failed: HTTP 503"
        );
    }

    #[test]
    fn failure_message_for_the_final_section_promises_no_further_text() {
        let last = section_failure_message(4, 5, "HTTP 503");
        assert!(last.contains("final section (5 of 5)"));
        // Nothing follows the last section, so it must not claim a remainder.
        assert!(!last.contains("the rest of the passage"));
    }
}

/// Extract envelope with default fallback
fn extract_envelope_or_default(wav_bytes: &[u8]) -> audio::AmplitudeEnvelope {
    audio::extract_envelope(wav_bytes, 40).unwrap_or_else(|_| audio::AmplitudeEnvelope {
        values: vec![0.5; 40],
        duration_ms: 2000,
    })
}

/// Record telemetry sample
fn record_telemetry(
    telemetry_state: &State<'_, Mutex<telemetry::TelemetryLog>>,
    engine: &str,
    voice: &str,
    text_len: usize,
    duration_ms: u64,
) {
    telemetry::record_sample(telemetry_state, engine, voice, text_len, duration_ms);
}

/// Add history entry with synthesis metadata
fn add_history_with_metadata(
    history: &State<'_, Mutex<HistoryLog>>,
    text: &str,
    engine: &str,
    voice: &str,
    duration_ms: u64,
    path: Option<String>,
    batch_id: Option<String>,
    synthesis_ms: u64,
    extra_metadata: Option<HashMap<String, serde_json::Value>>,
) {
    let mut metadata = extra_metadata.unwrap_or_default();
    metadata.insert("synthesis_ms".to_string(), serde_json::json!(synthesis_ms));
    history::add_entry_with_batch(
        history,
        text,
        engine,
        voice,
        duration_ms,
        path,
        batch_id,
        metadata,
    );
}

/// Emit audio-ready event with base64 encoded audio
fn emit_audio_ready(app: &AppHandle, speech: &SpeechAudio, text: &str, abort_generation: u32) {
    emit_audio_fragment(app, speech, 0, 1, text.to_owned(), abort_generation, None);
}

/// Emit audio-fragment-ready event for streaming playback.
///
/// `abort_generation` is the calling reading's own generation. The frontend
/// enqueues `audio-fragment-ready` unconditionally (unlike `audio-stream-chunk`,
/// which honours `_streamStopped`), so a fragment belonging to a reading the
/// user replaced would play over the replacement. Checking the generation —
/// not just `ABORT_REQUESTED`, which the replacement clears as it starts —
/// keeps that cancelled audio out of the new reading.
fn emit_audio_fragment(
    app: &AppHandle,
    speech: &SpeechAudio,
    index: usize,
    total: usize,
    text: String,
    abort_generation: u32,
    browser_reading_id: Option<&str>,
) {
    let wav_bytes = &speech.bytes;
    use base64::{engine::general_purpose, Engine as _};
    let encoded = general_purpose::STANDARD.encode(wav_bytes);
    // `total` is never 0 in practice; guard anyway so an empty fragment list
    // cannot wrap `total - 1` into a bogus terminal marker.
    let is_final = total == 0 || index + 1 >= total;
    // Checked after the encode (which is slow, and takes longer for long
    // fragments): a retrigger can land during it, leaving this fragment
    // belonging to a reading the user replaced.
    if !reading_may_publish(abort_generation, crate::ABORT_GENERATION.load(Ordering::SeqCst)) {
        return;
    }
    if let Err(e) = app.emit(
        "audio-fragment-ready",
        AudioFragmentEvent {
            browser_reading_id: browser_reading_id.map(str::to_owned),
            audio_base64: encoded,
            fragment_index: index,
            fragment_total: total,
            is_final,
            text,
            captions: speech.captions.clone(),
        },
    ) {
        log::warn!("Failed to emit audio-fragment-ready: {}", e);
    }
}

/// Emit one PCM chunk of streaming synthesis audio to the frontend.
/// A terminal zero-byte chunk with `is_final: true` marks end-of-stream.
fn emit_audio_stream_chunk(
    app: &AppHandle,
    meta: &AudioFormatMeta,
    pcm: &[u8],
    fragment_index: usize,
    is_final: bool,
    text: Option<&str>,
    fragment_duration_ms: Option<u64>,
    captions: Option<&CaptionAlignment>,
    abort_generation: u32,
    browser_reading_id: Option<&str>,
) {
    use base64::{engine::general_purpose, Engine as _};
    let encoded = general_purpose::STANDARD.encode(pcm);
    // Checked after the encode: a retrigger during it, or a replacement that has
    // already cleared `ABORT_REQUESTED`, would otherwise let this chunk of a
    // cancelled reading reach the new reading's scheduler.
    if !reading_may_publish(abort_generation, crate::ABORT_GENERATION.load(Ordering::SeqCst)) {
        return;
    }
    let event = AudioStreamChunkEvent {
        browser_reading_id: browser_reading_id.map(str::to_owned),
        audio_base64: encoded,
        sample_rate: meta.sample_rate,
        channels: meta.channels,
        bits_per_sample: meta.bits_per_sample,
        fragment_index,
        is_final,
        text: text.map(str::to_owned),
        fragment_duration_ms,
        captions: captions.cloned(),
    };
    if let Err(e) = app.emit("audio-stream-chunk", event) {
        log::warn!("Failed to emit audio-stream-chunk: {}", e);
    }
}

/// Drain a [`ChunkStream`], forwarding each PCM chunk to `on_chunk` while
/// accumulating them into a single WAV container (for history/cache/HUD).
///
/// `on_chunk(chunk_index, pcm, is_final)` fires once per received chunk plus a
/// terminal `(n, &[], true)` end-of-stream marker. Returns the wrapped WAV
/// bytes and time-to-first-audio in milliseconds.
/// How long the drain waits for the next chunk before re-checking cancellation.
/// A producer that stalls (a wedged daemon, a hung HTTP body) would otherwise
/// block `recv()` forever, keeping the shared reading lock and delaying — or
/// defeating — the replacement a retrigger asked for.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn drain_chunk_stream(
    stream: ChunkStream,
    mut on_chunk: impl FnMut(usize, &[u8], bool, Option<&CaptionAlignment>),
) -> Result<(SpeechAudio, u64), String> {
    let start = Instant::now();
    let meta = stream.meta.clone();
    let mut accumulated: Vec<u8> = Vec::new();
    let mut chunk_index = 0usize;
    let mut captions = None;
    let mut ttfa_ms: Option<u64> = None;

    loop {
        // Dropping the receiver is what actually cancels a streaming request:
        // every producer stops pulling its source once its send fails.
        if crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
            log::info!(
                "[TTS][stream] aborted by user after {} bytes",
                accumulated.len()
            );
            drop(stream);
            return Ok((Vec::new().into(), ttfa_ms.unwrap_or(0)));
        }
        // Poll rather than block so the check above runs even when no chunk is
        // coming. A timeout is not an error: it just means nothing arrived yet.
        let item = match stream.recv_timeout(DRAIN_POLL_INTERVAL) {
            Ok(item) => item,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => None,
        };
        match item {
            Some(ChunkItem::Pcm(bytes)) => {
                if ttfa_ms.is_none() {
                    ttfa_ms = Some(start.elapsed().as_millis() as u64);
                }
                accumulated.extend_from_slice(&bytes);
                on_chunk(chunk_index, &bytes, false, None);
                chunk_index += 1;
            }
            Some(ChunkItem::Captions(value)) if value.validate().is_ok() => {
                on_chunk(chunk_index, &[], false, Some(&value));
                captions = Some(value);
            }
            Some(ChunkItem::Captions(_)) | Some(ChunkItem::ClearCaptions) => {
                log::warn!("Clearing rejected live stream captions; continuing audio");
                if let Some(mut previous) = captions.take() {
                    // None means "no update" on the existing live wire. Send
                    // an empty valid snapshot to clear both live consumers,
                    // while retaining None for saved/cache metadata.
                    previous.words.clear();
                    on_chunk(chunk_index, &[], false, Some(&previous));
                }
            }
            Some(ChunkItem::Failed(reason)) => {
                log::error!(
                    "[TTS][stream] failed during receive: {} ({} bytes received)",
                    reason,
                    accumulated.len()
                );
                return Err(format!("Streaming synthesis failed mid-stream: {reason}"));
            }
            None => break,
        }
    }

    if accumulated.is_empty() {
        log::error!("[TTS][stream] failed during receive: stream ended with no audio");
        return Err("Streaming synthesis produced no audio".to_string());
    }

    // The stream can end (EOF) just as an abort lands, so re-check before the
    // terminal marker: it tells the player "no more audio is coming", which must
    // not be armed for a reading that was replaced.
    if crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
        log::info!("[TTS][stream] aborted at end-of-stream; suppressing terminal marker");
        drop(stream);
        return Ok((Vec::new().into(), ttfa_ms.unwrap_or(0)));
    }

    // Terminal end-of-stream marker so the frontend knows no more chunks follow.
    on_chunk(chunk_index, &[], true, captions.as_ref());

    let total_ms = start.elapsed().as_millis() as u64;
    log::info!(
        "[TTS][stream] ended: {} PCM bytes in {} chunks, {}ms total, TTFA {}ms",
        accumulated.len(),
        chunk_index,
        total_ms,
        ttfa_ms.unwrap_or(0)
    );
    Ok((
        SpeechAudio {
            bytes: pcm_to_wav(&accumulated, &meta),
            captions,
        },
        ttfa_ms.unwrap_or(0),
    ))
}

/// Whether a fresh synthesis may play as live `audio-stream-chunk` events.
/// The frontend PCM scheduler wires straight into `ctx.destination` with no
/// effect chain, so an enabled profile effect must force the fragment path,
/// where `buildPlaybackUrl` runs `effect.process` before playback.
fn allows_streaming_playback(
    streaming_enabled: bool,
    backend_streams: bool,
    effects_enabled: bool,
) -> bool {
    streaming_enabled && backend_streams && !effects_enabled
}

/// Synthesize via the backend's streaming path, forwarding each PCM chunk to
/// the frontend as an `audio-stream-chunk` event while accumulating the full
/// payload into a WAV container for history/cache/envelope handling.
async fn synthesize_streaming_and_emit(
    app: &AppHandle,
    backend: Arc<Box<dyn TtsBackend>>,
    text: String,
    voice: String,
    fragment_index: usize,
    emit_final: bool,
    abort_generation: u32,
    browser_reading_id: Option<&str>,
) -> Result<SpeechAudio, String> {
    let browser_reading_id = browser_reading_id.map(str::to_owned);
    let emitted_audio = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let cancelled = || crate::ABORT_REQUESTED.load(Ordering::Relaxed)
        || abort_generation != crate::ABORT_GENERATION.load(Ordering::SeqCst);
    let result = retry_synthesis(|| {
        let app = app.clone();
        let backend = backend.clone();
        let text = text.clone();
        let voice = voice.clone();
        let emitted_audio = emitted_audio.clone();
        let browser_reading_id = browser_reading_id.clone();
        async move { tokio::task::spawn_blocking(move || {
        let opened_at = Instant::now();
        log::info!(
            "[TTS][stream] opening streaming synthesis ({} chars, voice '{}', fragment {})",
            text.len(),
            voice,
            fragment_index
        );

        let stream = match backend.synthesize_streaming(&text, &voice) {
            Ok(s) => s,
            Err(e) => {
                log::error!(
                    "[TTS][stream] failed during opening: {} (0 bytes received)",
                    e
                );
                return Err(e.to_string());
            }
        };

        let meta = stream.meta.clone();
        let mut pcm_bytes = 0u64;
        drain_chunk_stream(stream, |chunk_idx, pcm, is_final, captions| {
            if !pcm.is_empty() {
                emitted_audio.store(true, Ordering::SeqCst);
            }
            pcm_bytes += pcm.len() as u64;
            if !is_final && chunk_idx == 0 {
                log::info!(
                    "[TTS][stream] first chunk playing after {}ms",
                    opened_at.elapsed().as_millis()
                );
            }
            // The player treats a final marker as "no more audio is coming" and
            // arms its completion timer, so only the last fragment sends one.
            let bytes_per_second = u64::from(meta.sample_rate)
                * u64::from(meta.channels)
                * u64::from(meta.bits_per_sample / 8);
            let duration_ms = if is_final && bytes_per_second > 0 {
                Some(pcm_bytes * 1000 / bytes_per_second)
            } else {
                None
            };
            emit_audio_stream_chunk(
                &app,
                &meta,
                pcm,
                fragment_index,
                is_final && emit_final,
                (chunk_idx == 0).then_some(text.as_str()),
                duration_ms,
                captions,
                abort_generation,
                browser_reading_id.as_deref(),
            );
        })
    })
    .await
    .map_err(|e| format!("Task join error: {e}"))?
    .map(|(wav, _ttfa_ms)| wav) }
    }, cancelled, || !emitted_audio.load(Ordering::SeqCst) && !backend.failure_is_final(), Duration::from_millis(500)).await;
    if cancelled() { Ok(Vec::new().into()) } else { result }
}

/// Save audio to history storage and return path
fn save_to_history_storage(
    config: &State<'_, Mutex<AppConfig>>,
    speech: &SpeechAudio,
    engine_id: &str,
    voice_name: &str,
    audio_ext: &str,
) -> Option<String> {
    let wav_bytes = &speech.bytes;
    let history_config = config.lock().unwrap().history.clone();
    let audio_ext = if wav_bytes.starts_with(b"RIFF") {
        "wav"
    } else {
        audio_ext
    };
    let path = crate::history::save_audio_to_storage(
        &history_config,
        wav_bytes,
        engine_id,
        voice_name,
        audio_ext,
    );
    if let Some(path) = &path {
        if let Some(captions) = &speech.captions {
            if let Err(error) = write_sidecar(path, captions) {
                log::warn!("Cannot save caption sidecar: {error}");
            }
        } else {
            crate::tts::captions::remove_sidecar(path);
        }
    }
    path
}

/// Cache audio for replay
fn cache_audio(app: &AppHandle, speech: &SpeechAudio, text: &str) {
    let wav_bytes = &speech.bytes;
    let cache = app.state::<Mutex<CachedAudio>>();
    let mut cache = cache.lock().unwrap();
    cache.wav_bytes = Some(wav_bytes.to_vec());
    cache.text = Some(text.to_string());
    cache.captions = speech.captions.clone();
}

/// Saved audio (and caption sidecar) of the newest successful history entry
/// with this exact text, engine and voice. `None` when no entry matches or its
/// file is gone or unreadable — the caller then synthesizes as usual.
fn find_cached_speech(
    history: &Mutex<HistoryLog>,
    text: &str,
    engine: &str,
    voice: &str,
) -> Option<SpeechAudio> {
    let path = history
        .lock()
        .unwrap()
        .entries()
        .iter()
        .rev()
        .filter(|e| e.success && e.text == text && e.voice == voice && e.tts_engine == engine)
        .find_map(|e| {
            e.output_path
                .clone()
                .filter(|path| std::path::Path::new(path).exists())
        })?;
    match std::fs::read(&path) {
        Ok(bytes) => Some(SpeechAudio {
            bytes,
            captions: read_sidecar(&path),
        }),
        Err(e) => {
            log::warn!(
                "[TTS] Saved audio unreadable ({}): {}. Re-synthesizing.",
                path,
                e
            );
            None
        }
    }
}

/// Saved audio for a whole reading: one entry with this text, or else every
/// fragment `speak_queued` would split it into, joined in order.
fn find_cached_reading(
    history: &Mutex<HistoryLog>,
    text: &str,
    pagination_config: &crate::config::PaginationConfig,
    engine: &str,
    voice: &str,
) -> Option<SpeechAudio> {
    if let Some(speech) = find_cached_speech(history, text, engine, voice) {
        return Some(speech);
    }
    let fragments = pagination::paginate_text(text, pagination_config);
    if fragments.len() < 2 {
        return None;
    }
    let parts = fragments
        .into_iter()
        .map(|f| Some((find_cached_speech(history, &f.text, engine, voice)?, f.text)))
        .collect::<Option<Vec<_>>>()?;
    crate::tts::captions::concat_speech(parts)
        .map_err(|e| log::warn!("[TTS] Cannot join saved fragments: {e}. Re-synthesizing."))
        .ok()
}

/// Play audio that history already holds: no new entry, file or telemetry
/// sample, since nothing was synthesized.
fn replay_saved_audio(app: &AppHandle, speech: &SpeechAudio, text: &str, abort_generation: u32) {
    cache_audio(app, speech, text);
    hud::show_hud(
        app,
        extract_envelope_or_default(&speech.bytes),
        Some(text.to_string()),
    );
    emit_audio_ready(app, speech, text, abort_generation);
}

// ── speak_now ───────────────────────────────────────────────────────────────

/// Manually trigger TTS for the given text (or current clipboard if empty).
#[tauri::command]
pub async fn speak_now(
    app: AppHandle,
    config: State<'_, Mutex<AppConfig>>,
    _player: State<'_, Mutex<AudioPlayer>>,
    history: State<'_, Mutex<HistoryLog>>,
    telemetry_state: State<'_, Mutex<telemetry::TelemetryLog>>,
    text: Option<String>,
) -> Result<(), String> {
    speak_now_internal(
        app,
        config,
        _player,
        history,
        telemetry_state,
        text,
        None,
        true,
    )
    .await
}

/// Like `speak_now`, but always synthesizes fresh audio, even when history
/// already holds this text for the active engine and voice.
#[tauri::command]
pub async fn regenerate_now(
    app: AppHandle,
    config: State<'_, Mutex<AppConfig>>,
    player: State<'_, Mutex<AudioPlayer>>,
    history: State<'_, Mutex<HistoryLog>>,
    telemetry_state: State<'_, Mutex<telemetry::TelemetryLog>>,
    text: String,
) -> Result<(), String> {
    speak_now_internal(
        app,
        config,
        player,
        history,
        telemetry_state,
        Some(text),
        None,
        false,
    )
    .await
}

#[tauri::command]
pub async fn speak_now_with_profile(
    app: AppHandle,
    config: State<'_, Mutex<AppConfig>>,
    player: State<'_, Mutex<AudioPlayer>>,
    history: State<'_, Mutex<HistoryLog>>,
    telemetry_state: State<'_, Mutex<telemetry::TelemetryLog>>,
    text: Option<String>,
    profile_id: Option<String>,
) -> Result<(), String> {
    speak_now_internal(
        app,
        config,
        player,
        history,
        telemetry_state,
        text,
        profile_id,
        true,
    )
    .await
}

async fn speak_now_internal(
    app: AppHandle,
    config: State<'_, Mutex<AppConfig>>,
    _player: State<'_, Mutex<AudioPlayer>>,
    history: State<'_, Mutex<HistoryLog>>,
    telemetry_state: State<'_, Mutex<telemetry::TelemetryLog>>,
    text: Option<String>,
    _profile_id: Option<String>,
    reuse_saved_audio: bool,
) -> Result<(), String> {
    let text = get_text_or_clipboard(text)?;
    // Play page, hotkey, tray, selection and control server all land here;
    // unsanitized web text (tabs, NBSPs) breaks engine tokenizers like Kokoro's.
    // `on_retrigger` is read under the same lock because both need the config,
    // and every one of those entry points must honour the setting exactly as
    // double-copy does.
    let (text, interrupt) = {
        let cfg = config.lock().unwrap();
        (
            crate::sanitize::sanitize_text(&text, &cfg.sanitization),
            retrigger_interrupts(&cfg.playback.on_retrigger),
        )
    };
    if text.trim().is_empty() {
        return Err("Nothing to speak".into());
    }

    // Honour `on_retrigger` before waiting on the shared synthesis lock. A
    // running reading holds that lock for its whole duration, so aborting from
    // inside it could only wait for the old reading to finish — the "queue
    // after current" behaviour this setting exists to override.
    // A replacement adopts the generation its own abort produced, so a second
    // retrigger racing alongside cannot be mistaken for this call's own abort.
    let abort_generation = if interrupt {
        crate::do_abort_synthesis(&app)
    } else {
        crate::ABORT_GENERATION.load(Ordering::SeqCst)
    };

    let lock_state = app.state::<tokio::sync::Mutex<()>>();
    let _queue_lock = lock_state.lock().await;

    // Clear any previous abort request
    if abort_generation != crate::ABORT_GENERATION.load(Ordering::SeqCst) {
        return Ok(());
    }
    crate::ABORT_REQUESTED.store(false, Ordering::Relaxed);
    if abort_generation != crate::ABORT_GENERATION.load(Ordering::SeqCst) {
        return Ok(());
    }

    // Enter critical section for TTS synthesis
    let _synthesis_guard = SynthesisGuard::new(&app);

    let (
        active_backend,
        tts_config,
        output_config,
        pagination_config,
        post_process_config,
        streaming_enabled,
    ) = {
        let cfg = config.lock().unwrap();
        (
            cfg.tts.active_backend.clone(),
            cfg.tts.clone(),
            cfg.output.clone(),
            cfg.pagination.clone(),
            cfg.post_process.clone(),
            cfg.playback.streaming_enabled,
        )
    };

    // Optional LLM post-processing (e.g., Groq) — best-effort; falls back to
    // the input text on any failure so synthesis is never blocked.
    let text = crate::post_process::try_process(text, &post_process_config).await;
    // The user may have hit Stop while the post-processor was running. Abort
    // before spending time synthesizing audio nobody asked for any more.
    if crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
        log::info!("[TTS] Aborted during post-processing; nothing to synthesize");
        return Ok(());
    }
    let _ = app.emit("reading-started", &text);

    log_tts_debug("TTS", &format!("{:?}", active_backend), &text);

    let eff = resolve_effective(&tts_config);
    let voice = eff.voice.clone();

    let backend: Box<dyn TtsBackend> = create_backend_from_effective(&eff, &tts_config);
    let engine_str_for_cache = engine_str(&active_backend);

    let cached = if reuse_saved_audio {
        find_cached_reading(
            &history,
            &text,
            &pagination_config,
            &engine_str_for_cache,
            &voice,
        )
    } else {
        None
    };
    let cache_hit = cached.is_some();

    let synthesis_start = Instant::now();

    // Get telemetry estimate for progress display
    let (estimated_ms, confidence) =
        telemetry::get_estimate(&telemetry_state, &engine_str_for_cache, &voice, text.len());

    // Emit initial synthesis progress
    hud::emit_synthesis_progress(
        &app,
        estimated_ms,
        0,
        0,
        1,
        false,
        confidence,
        truncate_preview(&text, 50),
        text.len(),
        0,
    );

    // Show HUD with synthesizing indicator
    hud::show_hud_synthesizing(&app, Some(text.clone()));

    // Wrap backend in Arc for sharing across pagination calls
    let backend_arc = Arc::new(backend);

    // Streaming backends forward PCM chunks during synthesis in playback mode;
    // batch backends (and cache hits / file output / pagination) take the
    // untouched existing branches.
    let streaming_playback = allows_streaming_playback(
        streaming_enabled,
        backend_arc.supports_streaming(),
        eff.effects.enabled,
    ) && !cache_hit
        && !output_config.enabled;
    let (wav_bytes, already_streamed) = if let Some(speech) = cached {
        log::info!("[TTS] Replaying saved audio from history instead of synthesizing");
        (speech, false)
    } else if pagination::should_paginate(&text, &pagination_config) && !output_config.enabled {
        // Pagination comes first even for streaming engines: bounded fragments
        // keep each request under the provider's input limit (OpenAI rejects
        // anything past 4096 characters). Each fragment then streams on its own,
        // so long text is both accepted and low-latency. Batch fragments are
        // emitted per fragment here, so the frontend must not also receive the
        // concatenated audio (that would play the whole reading twice).
        let (wav, fragments_emitted) = synthesize_paginated(
            &app,
            backend_arc.clone(),
            &text,
            &voice,
            &active_backend,
            &telemetry_state,
            &pagination_config,
            &synthesis_start,
            estimated_ms,
            confidence,
            streaming_playback,
            abort_generation,
        )
        .await?;
        (wav, streaming_playback || fragments_emitted)
    } else if streaming_playback {
        let wav = synthesize_streaming_and_emit(
            &app,
            backend_arc.clone(),
            text.clone(),
            voice.clone(),
            0,
            true,
            abort_generation,
            None,
        )
        .await
        .map_err(|error| report_section_failure(&app, 0, 1, abort_generation, error))?;
        (wav, true)
    } else {
        // Simple synthesis. Short text, and any reading with pagination
        // disabled, takes this branch instead of synthesize_paginated — so it
        // needs the same reporting, or a failed hotkey/tray/selection reading
        // stays silent (the caller only logs the returned error).
        let wav = synthesize_async(backend_arc.clone(), text.clone(), voice.clone())
            .await
            .map_err(|error| report_section_failure(&app, 0, 1, abort_generation, error))?;
        (wav, false)
    };

    // Both the paginated and streaming paths return empty bytes when the user
    // aborts mid-synthesis. do_abort_synthesis has already stopped playback and
    // emitted synthesis-aborted, so there is nothing to save or play here.
    if crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
        log::info!("[TTS] Synthesis aborted; nothing to play or save");
        return Ok(());
    }

    // A retrigger can also land in the instant after the flag check above. This
    // reading is no longer current then: publishing would cache cancelled audio
    // (poisoning Replay) and emit it over the reading the user just started.
    if !reading_may_publish(
        abort_generation,
        crate::ABORT_GENERATION.load(Ordering::SeqCst),
    ) {
        log::info!("[TTS] Reading replaced; nothing to play or save");
        return Ok(());
    }

    let synthesis_duration = synthesis_start.elapsed();
    let synthesis_ms = synthesis_duration.as_millis() as u64;

    // A replay took no synthesis time; recording it would skew estimates.
    if !cache_hit {
        record_telemetry(
            &telemetry_state,
            &engine_str(&active_backend),
            &voice,
            text.len(),
            synthesis_ms,
        );
    }

    if crate::logging::is_debug_mode() {
        log::debug!("[TTS] Synthesis completed in {:?}", synthesis_duration);
        log::debug!("[TTS] Audio size: {} bytes", wav_bytes.bytes.len());
    }

    // Handle file output mode (a replay still writes the requested file)
    if output_config.enabled && !output_config.directory.is_empty() {
        return handle_file_output(
            &app,
            &config,
            &history,
            &wav_bytes,
            &text,
            &voice,
            &active_backend,
            &output_config,
            synthesis_ms,
            abort_generation,
        );
    }

    if cache_hit {
        replay_saved_audio(&app, &wav_bytes, &text, abort_generation);
        return Ok(());
    }

    // Normal playback mode
    handle_playback_output(
        &app,
        &config,
        &history,
        &wav_bytes,
        &text,
        &voice,
        &active_backend,
        &tts_config,
        backend_arc,
        synthesis_ms,
        already_streamed,
        eff.voice_label.as_deref(),
        abort_generation,
    )
}

/// Synthesize paginated text with progress updates.
///
/// Returns the concatenated [`SpeechAudio`] plus a flag reporting whether each
/// fragment was already forwarded to the frontend as an `audio-fragment-ready`
/// event. When the flag is set the caller must not emit the concatenated audio
/// again, or the reading plays twice.
#[allow(clippy::too_many_arguments)]
async fn synthesize_paginated(
    app: &AppHandle,
    backend_arc: Arc<Box<dyn TtsBackend>>,
    text: &str,
    voice: &str,
    active_backend: &crate::config::TtsEngine,
    telemetry_state: &State<'_, Mutex<telemetry::TelemetryLog>>,
    pagination_config: &crate::config::PaginationConfig,
    synthesis_start: &Instant,
    _total_estimate: Option<u64>,
    _avg_confidence: f32,
    streaming: bool,
    abort_generation: u32,
) -> Result<(SpeechAudio, bool), String> {
    // Honour the user's configured fragment size. Falling back to the default
    // here used to split every Play-page/hotkey/tray reading at 600 chars
    // regardless of settings, and disagreed with find_cached_reading's lookup.
    let fragments = pagination::paginate_text(text, pagination_config);

    if fragments.len() <= 1 {
        // Only one fragment — fall back to normal synthesis. Routed through the
        // same reporting as the loop below: a failure here used to return a raw
        // error that double-copy only console.error'd, so a short passage that
        // failed was silent.
        let speech = if streaming {
            synthesize_streaming_and_emit(
                app,
                backend_arc.clone(),
                text.to_string(),
                voice.to_string(),
                0,
                true,
                abort_generation,
                None,
            )
            .await
        } else {
            synthesize_async(backend_arc.clone(), text.to_string(), voice.to_string()).await
        }
        .map_err(|error| {
            report_section_failure(app, 0, fragments.len().max(1), abort_generation, error)
        })?;
        return Ok((speech, streaming));
    }

    log::info!(
        "[TTS] Paginating text ({} chars) into {} fragments",
        text.len(),
        fragments.len()
    );

    let char_counts: Vec<usize> = fragments.iter().map(|f| f.text.len()).collect();
    let (total_estimate, avg_confidence, _) = telemetry::get_estimate_paginated(
        telemetry_state,
        &engine_str(active_backend),
        voice,
        &char_counts,
    );

    let mut fragment_audio = Vec::new();
    // Batch fragments are streamed to the frontend as they are synthesized so
    // playback starts on fragment 1 instead of after the entire passage is
    // rendered. `already_emitted` tracks whether the caller may still emit the
    // concatenated audio (streaming backends emit their own PCM chunks).
    let mut already_emitted = false;
    for (i, fragment) in fragments.iter().enumerate() {
        // Check for abort
        if crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
            log::info!("[TTS] Pagination aborted by user");
            return Ok((Vec::new().into(), true));
        }

        // Emit progress update
        let elapsed = synthesis_start.elapsed().as_millis() as u64;
        let processed_chars: usize = fragments.iter().take(i).map(|f| f.text.len()).sum();
        hud::emit_synthesis_progress(
            app,
            total_estimate,
            elapsed,
            i,
            fragments.len(),
            true,
            avg_confidence,
            truncate_preview(&fragment.text, 50),
            text.len(),
            processed_chars,
        );

        log::info!(
            "[TTS] Synthesizing fragment {}/{} ({} chars)",
            i + 1,
            fragments.len(),
            fragment.text.len()
        );

        let frag_wav = if streaming {
            synthesize_streaming_and_emit(
                app,
                backend_arc.clone(),
                fragment.text.clone(),
                voice.to_string(),
                i,
                i + 1 == fragments.len(),
                abort_generation,
                None,
            )
            .await
        } else {
            synthesize_async(
                backend_arc.clone(),
                fragment.text.clone(),
                voice.to_string(),
            )
            .await
        }
        .map_err(|error| report_section_failure(app, i, fragments.len(), abort_generation, error))?;

        if crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
            return Ok((Vec::new().into(), true));
        }

        // Hand the finished fragment to the player immediately. The frontend
        // queue plays fragments in order, so later ones simply wait their turn.
        if !streaming {
            if i == 0 {
                hud::show_hud(app, extract_envelope_or_default(&frag_wav.bytes), Some(text.to_string()));
            }
            emit_audio_fragment(
                app,
                &frag_wav,
                i,
                fragments.len(),
                fragment.text.clone(),
                abort_generation,
                None,
            );
            already_emitted = true;
        }

        fragment_audio.push((frag_wav, fragment.text.clone()));
    }

    let assembled = crate::tts::captions::concat_speech(fragment_audio)
        .map_err(|e| format!("Failed to concatenate audio fragments: {e}"))?;
    Ok((assembled, already_emitted))
}

/// Handle file output mode
fn handle_file_output(
    app: &AppHandle,
    _config: &State<'_, Mutex<AppConfig>>,
    history: &State<'_, Mutex<HistoryLog>>,
    speech: &SpeechAudio,
    text: &str,
    voice: &str,
    active_backend: &crate::config::TtsEngine,
    output_config: &crate::config::OutputConfig,
    synthesis_ms: u64,
    abort_generation: u32,
) -> Result<(), String> {
    let wav_bytes = &speech.bytes;

    // Re-check before writing anything: this path persists a file, a caption
    // sidecar and a history row, all of which must not appear for a reading the
    // user cancelled after the caller's check.
    if !reading_may_publish(
        abort_generation,
        crate::ABORT_GENERATION.load(Ordering::SeqCst),
    ) {
        log::info!("[TTS] Reading replaced; skipping file output");
        return Ok(());
    }

    let output_dir = std::path::Path::new(&output_config.directory);
    std::fs::create_dir_all(output_dir)
        .map_err(|e| format!("Failed to create output directory: {e}"))?;

    let mut filename =
        config::expand_filename_pattern(&output_config.filename_pattern, voice, text);

    let audio_bytes = if output_config.format_config.format != crate::config::AudioFormat::Wav {
        if filename.to_lowercase().ends_with(".wav") {
            let new_ext = output_config.format_config.format.default_extension();
            filename = format!(
                "{}.{}",
                filename.trim_end_matches(".wav").trim_end_matches(".WAV"),
                new_ext
            );
        }
        audio::convert_audio_format(wav_bytes, &output_config.format_config)
            .map_err(|e| format!("Failed to convert audio format: {e}"))?
    } else {
        wav_bytes.to_vec()
    };

    // `convert_audio_format` shells out (FFmpeg) for non-WAV formats, which is
    // slow enough for a Stop to land mid-way. Re-check before the *persistent*
    // work below — the destination file, its caption sidecar and the history
    // row must not appear for a reading the user cancelled during conversion.
    if !reading_may_publish(
        abort_generation,
        crate::ABORT_GENERATION.load(Ordering::SeqCst),
    ) {
        log::info!("[TTS] Reading replaced during format conversion; not writing output");
        return Ok(());
    }

    let output_path = output_dir.join(&filename);
    std::fs::write(&output_path, &audio_bytes)
        .map_err(|e| format!("Failed to write audio file: {e}"))?;

    crate::tts::captions::remove_sidecar(&output_path.to_string_lossy());
    if let Some(captions) = &speech.captions {
        if let Err(error) = write_sidecar(&output_path.to_string_lossy(), captions) {
            log::warn!("Cannot save caption sidecar: {error}");
        }
    }
    log::info!("Saved TTS audio to: {}", output_path.display());

    if crate::logging::is_debug_mode() {
        log::debug!("[TTS] Output file: {} bytes", audio_bytes.len());
    }

    let audio_duration_ms = audio::extract_envelope(wav_bytes, 1)
        .map(|e| e.duration_ms)
        .unwrap_or(0);

    add_history_with_metadata(
        history,
        text,
        &engine_str(active_backend),
        voice,
        audio_duration_ms,
        Some(output_path.to_string_lossy().into_owned()),
        None,
        synthesis_ms,
        None,
    );
    let _ = app.emit("history-updated", ());

    Ok(())
}

/// Handle normal playback output
#[allow(clippy::too_many_arguments)]
fn handle_playback_output(
    app: &AppHandle,
    config: &State<'_, Mutex<AppConfig>>,
    history: &State<'_, Mutex<HistoryLog>>,
    speech: &SpeechAudio,
    text: &str,
    voice: &str,
    active_backend: &crate::config::TtsEngine,
    tts_config: &crate::config::TtsConfig,
    backend_arc: Arc<Box<dyn TtsBackend>>,
    synthesis_ms: u64,
    already_streamed: bool,
    voice_label: Option<&str>,
    abort_generation: u32,
) -> Result<(), String> {
    let wav_bytes = &speech.bytes;
    let envelope = extract_envelope_or_default(wav_bytes);

    // Re-check before the persistent work below. The caller checked too, but a
    // Stop can land in between; committing then would leave a history row, a
    // saved file and a Replay cache entry for a reading the user cancelled.
    if !reading_may_publish(
        abort_generation,
        crate::ABORT_GENERATION.load(Ordering::SeqCst),
    ) {
        log::info!("[TTS] Reading replaced; skipping output commit");
        return Ok(());
    }

    let engine_id = engine_identifier(active_backend);
    let voice_name = voice_display_name(active_backend, tts_config, voice, voice_label);
    let audio_ext = backend_arc.file_extension().to_string();

    let history_path = save_to_history_storage(config, speech, &engine_id, &voice_name, &audio_ext);

    add_history_with_metadata(
        history,
        text,
        &engine_str(active_backend),
        voice,
        envelope.duration_ms,
        history_path,
        None,
        synthesis_ms,
        None,
    );
    let _ = app.emit("history-updated", ());

    // Cache audio for replay
    cache_audio(app, speech, text);

    // Emit audio to frontend for browser-native playback. Skipped when PCM
    // chunks were already forwarded by the streaming synthesis path.
    if !already_streamed {
        hud::show_hud(app, envelope, Some(text.to_string()));
        emit_audio_ready(app, speech, text, abort_generation);
    }

    Ok(())
}

// ── speak_queued ────────────────────────────────────────────────────────────

/// Speak text with fragment queue support for long texts.
/// Automatically paginates long texts and plays fragments sequentially.
#[tauri::command]
pub async fn speak_queued(
    app: AppHandle,
    config: State<'_, Mutex<AppConfig>>,
    _player: State<'_, Mutex<AudioPlayer>>,
    history: State<'_, Mutex<HistoryLog>>,
    queue: State<'_, Mutex<FragmentQueue>>,
    telemetry_state: State<'_, Mutex<telemetry::TelemetryLog>>,
    text: Option<String>,
) -> Result<(), String> {
    speak_queued_with_owner(app, config, _player, history, queue, telemetry_state, text, None).await
}

pub async fn speak_queued_with_owner(
    app: AppHandle,
    config: State<'_, Mutex<AppConfig>>,
    _player: State<'_, Mutex<AudioPlayer>>,
    history: State<'_, Mutex<HistoryLog>>,
    queue: State<'_, Mutex<FragmentQueue>>,
    telemetry_state: State<'_, Mutex<telemetry::TelemetryLog>>,
    text: Option<String>,
    browser_reading_id: Option<String>,
) -> Result<(), String> {
    let text = get_text_or_clipboard(text)?;
    if text.trim().is_empty() {
        return Err("Nothing to speak".into());
    }

    // Honour `on_retrigger` before waiting on the shared synthesis lock. A
    // running reading holds that lock for its whole duration, so aborting from
    // inside it could only wait for the old reading to finish — which is the
    // "queue after current" behaviour this setting exists to override.
    // `do_abort_synthesis` only touches `std::sync` primitives, so calling it
    // here cannot deadlock against the tokio lock, and it emits `playback-stop`,
    // which is what actually silences the webview audio.
    let interrupt = {
        let cfg = config.lock().unwrap();
        retrigger_interrupts(&cfg.playback.on_retrigger)
    };
    // A replacement adopts the generation its own abort produced. Using the
    // value from this call's increment (rather than re-reading the atomic) is
    // what stops a second retrigger racing alongside from being mistaken for
    // our own abort and silently swallowing the reading the user just made.
    let abort_generation = if interrupt {
        crate::do_abort_synthesis(&app)
    } else {
        crate::ABORT_GENERATION.load(Ordering::SeqCst)
    };

    let lock_state = app.state::<tokio::sync::Mutex<()>>();
    let _queue_lock = lock_state.lock().await;

    let (active_backend, tts_config, pagination_config, post_process_config, streaming_enabled) = {
        let cfg = config.lock().unwrap();
        (
            cfg.tts.active_backend.clone(),
            cfg.tts.clone(),
            cfg.pagination.clone(),
            cfg.post_process.clone(),
            cfg.playback.streaming_enabled,
        )
    };

    // Clear any previous abort request, the same way speak_now_internal does.
    // Both commands feed drain_chunk_stream, which now honours the flag, so a
    // stale `true` from an earlier abort would kill this run's first stream.
    if abort_generation != crate::ABORT_GENERATION.load(Ordering::SeqCst) {
        return Ok(());
    }
    crate::ABORT_REQUESTED.store(false, Ordering::Relaxed);
    if abort_generation != crate::ABORT_GENERATION.load(Ordering::SeqCst) {
        return Ok(());
    }

    let _synthesis_guard = SynthesisGuard::new(&app);

    // Optional LLM post-processing — best-effort; falls back on failure.
    let text = crate::post_process::try_process(text, &post_process_config).await;
    if crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
        return Ok(());
    }
    let _ = app.emit("reading-started", &text);

    log_tts_debug("Queue", &format!("{:?}", active_backend), &text);

    // Show HUD with synthesizing indicator
    hud::show_hud_synthesizing(&app, Some(text.clone()));

    // Paginate text
    let fragments = pagination::paginate_text(&text, &pagination_config);

    if crate::logging::is_debug_mode() {
        log::debug!("[Queue] Created {} fragments", fragments.len());
    }

    let eff = resolve_effective(&tts_config);
    let voice = eff.voice.clone();

    // Get telemetry estimates
    let char_counts: Vec<usize> = fragments.iter().map(|f| f.text.len()).collect();
    let (total_estimate, avg_confidence, _) = telemetry::get_estimate_paginated(
        &telemetry_state,
        &engine_str(&active_backend),
        &voice,
        &char_counts,
    );

    let synthesis_start = Instant::now();
    let total_chars = text.len();

    // Emit initial synthesis progress
    hud::emit_synthesis_progress(
        &app,
        total_estimate,
        0,
        0,
        fragments.len(),
        fragments.len() > 1,
        avg_confidence,
        truncate_preview(&text, 50),
        total_chars,
        0,
    );

    // Clear previous queue and add new fragments. `clear()` deliberately leaves
    // the stop flag alone (the running loop may be watching it), so reset it
    // here: a Stop/Abort earlier in the session must not silence this reading.
    {
        let q = queue.lock().unwrap();
        q.clear();
        q.clear_stop_flag();
        q.add_fragments(fragments.clone());
    }

    // Emit pagination started event
    let _ = app.emit(
        "pagination:started",
        PaginationEvent {
            total: fragments.len(),
            current_index: 0,
            is_paginated: fragments.len() > 1,
        },
    );

    // Generate batch ID for grouping history entries
    let total = fragments.len();
    let batch_id = if total > 1 {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Some(format!("batch_{}", ts))
    } else {
        None
    };

    let engine_str_val = engine_str(&active_backend);
    let engine_id_val = engine_identifier(&active_backend);

    // Pick the transport for the WHOLE reading, once. `supports_streaming` for a
    // local CLI reflects whether its daemon is warm *right now*, so deciding per
    // fragment let a reading start on the batch path (daemon still loading) and
    // switch to streamed playback at a later fragment — the frontend then had
    // batch audio playing while a PCM scheduler took over, and completion
    // signals only ever reached one of them. Reusing one backend also keeps
    // every fragment on the same engine options.
    let reading_backend: Arc<Box<dyn TtsBackend>> =
        Arc::new(create_backend_from_effective(&eff, &tts_config));

    // Repeated text replays saved audio — only when history holds every
    // fragment, so a reading never mixes saved and fresh parts nor leaves a
    // partial batch in history.
    let mut saved: Option<std::vec::IntoIter<SpeechAudio>> = fragments
        .iter()
        .map(|f| find_cached_speech(&history, &f.text, &engine_str_val, &voice))
        .collect::<Option<Vec<_>>>()
        .map(Vec::into_iter);
    let replaying = saved.is_some();
    if replaying {
        log::info!("[Queue] Replaying saved audio for all {} fragments", total);
    }

    // Decided only after `replaying`: a replayed reading has no synthesis to
    // stream at all.
    let stream_reading = !replaying
        && allows_streaming_playback(
            streaming_enabled,
            reading_backend.supports_streaming(),
            eff.effects.enabled,
        );

    // Synthesize and play each fragment
    // Fragments are collected as well as played so the whole reading can be
    // cached for Replay (see the concat below).
    let mut fragment_audio_for_cache: Vec<(SpeechAudio, String)> = Vec::new();
    for (index, fragment) in fragments.iter().enumerate() {
        // Check if we should stop
        {
            let q = queue.lock().unwrap();
            if q.should_stop() || crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
                // Clear the queue either way; only the *event* depends on whether
                // this reading is still the current one. A replaced reading's
                // `pagination:stopped` would reach the replacement's still-empty
                // queue and mark it complete before it played anything.
                q.clear();
                if reading_may_publish(
                    abort_generation,
                    crate::ABORT_GENERATION.load(Ordering::SeqCst),
                ) {
                    log::info!("[Queue] Playback stopped by user");
                    let _ = app.emit(
                        "pagination:stopped",
                        PaginationEvent {
                            total,
                            current_index: index,
                            is_paginated: total > 1,
                        },
                    );
                } else {
                    log::info!("[Queue] Reading replaced; exiting quietly");
                }
                return Ok(());
            }
        }

        log::debug!("[Queue] Synthesizing fragment {} of {}", index + 1, total);

        // These UI/progress events are gated on the reading still being current.
        // A fragment that finishes after a replacement has started must not
        // overwrite the new reading's fragment index or progress in the HUD.
        let still_current =
            reading_may_publish(abort_generation, crate::ABORT_GENERATION.load(Ordering::SeqCst));

        // Emit fragment started event
        if still_current {
            let _ = app.emit(
                "pagination:fragment-started",
                PaginationEvent {
                    total,
                    current_index: index,
                    is_paginated: total > 1,
                },
            );
        }

        // Emit progress update
        if still_current {
            let elapsed = synthesis_start.elapsed().as_millis() as u64;
            let processed_chars: usize = fragments.iter().take(index).map(|f| f.text.len()).sum();
            hud::emit_synthesis_progress(
                &app,
                total_estimate,
                elapsed,
                index,
                total,
                true,
                avg_confidence,
                truncate_preview(&fragment.text, 50),
                total_chars,
                processed_chars,
            );
        }

        // Set current index in queue
        {
            let q = queue.lock().unwrap();
            q.set_current_index(index);
        }

        // Synthesize fragment — streaming-capable backends forward PCM chunks
        // as they arrive; batch backends take the untouched synthesize path.
        // `stream_reading` was fixed before the loop so this reading uses one
        // transport throughout (see above).
        let backend_arc = reading_backend.clone();
        let fragment_start = Instant::now();
        let wav_bytes = if let Some(speech) = saved.as_mut().and_then(Iterator::next) {
            Ok(speech)
        } else if stream_reading {
            synthesize_streaming_and_emit(
                &app,
                backend_arc.clone(),
                fragment.text.clone(),
                voice.clone(),
                index,
                index + 1 == total,
                abort_generation,
                browser_reading_id.as_deref(),
            )
            .await
        } else {
            synthesize_async(backend_arc.clone(), fragment.text.clone(), voice.clone()).await
        }.map_err(|error| report_section_failure(&app, index, total, abort_generation, error))?;
        let fragment_duration = fragment_start.elapsed();
        if crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
            // Aborted mid-fragment — the path almost every retrigger takes, since
            // a fragment in flight only notices the abort once it returns. Its
            // guard emits the matching `synthesis-state-change: false`.
            return Ok(());
        }

        if !replaying {
            record_telemetry(
                &telemetry_state,
                &engine_str_val,
                &voice,
                fragment.text.len(),
                fragment_duration.as_millis() as u64,
            );
        }

        // Store audio in queue
        {
            let q = queue.lock().unwrap();
            q.set_audio(index, wav_bytes.bytes.clone());
        }

        fragment_audio_for_cache.push((wav_bytes.clone(), fragment.text.clone()));

        log::debug!(
            "[Queue] Synthesized fragment {} ({} bytes)",
            index + 1,
            wav_bytes.bytes.len()
        );

        // Extract envelope
        let envelope = extract_envelope_or_default(&wav_bytes.bytes);

        // Transition HUD from synthesizing → playing on first fragment
        if index == 0 && !stream_reading {
            hud::show_hud(&app, envelope.clone(), Some(text.clone()));
        }

        // Save to history (a replay is already there). Guarded on the reading
        // still being current: an in-flight fragment finishes synthesizing even
        // after a retrigger set the flag, and its file, caption sidecar and
        // history row must not be written for a reading the user cancelled.
        if !replaying
            && reading_may_publish(abort_generation, crate::ABORT_GENERATION.load(Ordering::SeqCst))
        {
            let audio_ext = backend_arc.file_extension().to_string();
            let voice_name = voice_display_name(
                &active_backend,
                &tts_config,
                &voice,
                eff.voice_label.as_deref(),
            );
            let history_path = save_to_history_storage(
                &config,
                &wav_bytes,
                &engine_id_val,
                &voice_name,
                &audio_ext,
            );

            // Build metadata
            let mut metadata = HashMap::new();
            if total > 1 {
                metadata.insert("fragment_index".to_string(), serde_json::json!(index));
                metadata.insert("fragment_total".to_string(), serde_json::json!(total));
            }

            add_history_with_metadata(
                &history,
                &fragment.text,
                &engine_str_val,
                &voice,
                envelope.duration_ms,
                history_path,
                batch_id.clone(),
                fragment_duration.as_millis() as u64,
                Some(metadata),
            );
            let _ = app.emit("history-updated", ());
        }

        // Emit audio fragment for streaming playback. Skipped when the PCM
        // chunks were already forwarded by the streaming path above.
        // (`emit_audio_fragment` re-checks the generation itself.)
        if !stream_reading {
            emit_audio_fragment(&app, &wav_bytes, index, total, fragment.text.clone(), abort_generation, browser_reading_id.as_deref());
        }

        // Emit fragment events. Gated like the started/progress events above so
        // a replaced reading cannot overwrite the replacement's fragment state.
        if reading_may_publish(abort_generation, crate::ABORT_GENERATION.load(Ordering::SeqCst)) {
            let _ = app.emit(
                "pagination:fragment-ready",
                PaginationEvent {
                    total,
                    current_index: index,
                    is_paginated: total > 1,
                },
            );
            let _ = app.emit(
                "pagination:fragment-complete",
                PaginationEvent {
                    total,
                    current_index: index,
                    is_paginated: total > 1,
                },
            );
        }
    }

    // Every fragment synthesized. A reading that is no longer current must not
    // publish anything: its cached audio is a reading the user cancelled, and
    // its completion event would tell the frontend the *replacement* had ended,
    // finishing the replacement's still-empty queue before it played a note.
    // The generation differs here when this reading was replaced (a replacer
    // always waits on the shared lock, so it cannot be mid-flight) or when the
    // user stopped it in this tail window — both mean "do not publish".
    if !reading_may_publish(
        abort_generation,
        crate::ABORT_GENERATION.load(Ordering::SeqCst),
    ) {
        return Ok(());
    }

    // Clear queue (frontend handles streaming)
    {
        let q = queue.lock().unwrap();
        q.clear();
    }

    log::info!("[Queue] All {} fragments synthesized and streamed", total);

    // Cache the whole reading so Replay works. speak_queued streams per fragment
    // and the frontend never holds the full audio, so without this a Replay would
    // fall back to a stale speak_now cache or the last fragment's buffer.
    // `concat_speech` decodes every sample, so a retrigger can land during it;
    // re-check the generation before committing anything. A cancelled reading
    // must neither overwrite the cache with audio the user never finished nor
    // wipe a valid previous cache on a concat failure.
    let cached_reading = if fragment_audio_for_cache.is_empty() {
        None
    } else {
        Some(crate::tts::captions::concat_speech(fragment_audio_for_cache))
    };

    if reading_may_publish(
        abort_generation,
        crate::ABORT_GENERATION.load(Ordering::SeqCst),
    ) {
        match cached_reading {
            Some(Ok(speech)) => cache_audio(&app, &speech, &text),
            Some(Err(e)) => {
                *app.state::<Mutex<CachedAudio>>().lock().unwrap() = CachedAudio::default();
                log::warn!("[Queue] Cannot cache full reading for replay: {e}");
            }
            None => {}
        }

        let _ = app.emit(
            "pagination:complete",
            PaginationEvent {
                total,
                current_index: total - 1,
                is_paginated: total > 1,
            },
        );
    }

    Ok(())
}

// ── speak_history_entry ─────────────────────────────────────────────────────

/// Re-synthesize and play a history entry.
/// This creates new TTS audio using the current config and plays it.
#[tauri::command]
pub async fn speak_history_entry(
    app: AppHandle,
    config: State<'_, Mutex<AppConfig>>,
    history: State<'_, Mutex<HistoryLog>>,
    telemetry_state: State<'_, Mutex<telemetry::TelemetryLog>>,
    entry_id: String,
) -> Result<(), String> {
    // Look the entry up *before* any abort: an unknown or deleted id must not
    // stop whatever the user is currently hearing only to return "not found".
    let (text, original_voice) = {
        let hist = history.lock().unwrap();
        let entry = hist
            .get_by_id(&entry_id)
            .ok_or_else(|| format!("History entry not found: {}", entry_id))?;
        (entry.text.clone(), entry.voice.clone())
    };
    if text.trim().is_empty() {
        return Err("Nothing to speak".to_string());
    }

    // Join the shared cancellation protocol used by speak_now/speak_queued.
    // This command used to clear `ABORT_REQUESTED` unconditionally and skip the
    // reading lock, so a history re-speak during a retrigger could wipe the
    // abort flag the replaced reading was about to observe (letting it run on
    // and emit into the replacement) or outlive the replacement and emit a
    // stale `synthesis-state-change: false`.
    let interrupt = {
        let cfg = config.lock().unwrap();
        retrigger_interrupts(&cfg.playback.on_retrigger)
    };
    let abort_generation = if interrupt {
        crate::do_abort_synthesis(&app)
    } else {
        crate::ABORT_GENERATION.load(Ordering::SeqCst)
    };

    let lock_state = app.state::<tokio::sync::Mutex<()>>();
    let _queue_lock = lock_state.lock().await;

    if abort_generation != crate::ABORT_GENERATION.load(Ordering::SeqCst) {
        return Ok(());
    }
    // Safe to clear now: this reading holds the lock and is still current, so
    // no live reading is watching the flag.
    crate::ABORT_REQUESTED.store(false, Ordering::Relaxed);
    if abort_generation != crate::ABORT_GENERATION.load(Ordering::SeqCst) {
        return Ok(());
    }

    let _synthesis_guard = SynthesisGuard::new(&app);
    if crate::logging::is_debug_mode() {
        log::debug!("[IPC] speak_history_entry called (id: {})", entry_id);
    }

    let tts_config = {
        let cfg = config.lock().unwrap();
        cfg.tts.clone()
    };

    let active_backend = active_engine(&tts_config);

    let backend: Box<dyn TtsBackend> = create_backend(&active_backend, &tts_config);
    let voice = original_voice;
    let engine_str_val = engine_str(&active_backend);
    let engine_id = engine_identifier(&active_backend);
    let voice_name = voice_display_name(&active_backend, &tts_config, &voice, None);
    let audio_ext = backend.file_extension().to_string();

    // Get telemetry estimate
    let (estimated_ms, confidence) =
        telemetry::get_estimate(&telemetry_state, &engine_str_val, &voice, text.len());

    // Emit progress event
    hud::emit_synthesis_progress(
        &app,
        estimated_ms,
        0,
        0,
        1,
        false,
        confidence,
        truncate_preview(&text, 50),
        text.len(),
        0,
    );

    // Show HUD with synthesizing indicator
    hud::show_hud_synthesizing(&app, Some(text.clone()));

    let backend_arc = Arc::new(backend);
    let synthesis_start = Instant::now();

    // Synthesize
    let wav_bytes = synthesize_async(backend_arc.clone(), text.clone(), voice.clone()).await?;
    if crate::ABORT_REQUESTED.load(Ordering::Relaxed) {
        return Ok(());
    }
    // A retrigger can land in the instant after the flag check above. This
    // reading is no longer current then, so publishing would cache cancelled
    // audio and emit it over the reading the user just started.
    if !reading_may_publish(
        abort_generation,
        crate::ABORT_GENERATION.load(Ordering::SeqCst),
    ) {
        log::info!("[TTS] History reading replaced; nothing to play or save");
        return Ok(());
    }
    let synthesis_ms = synthesis_start.elapsed().as_millis() as u64;

    // Record telemetry
    record_telemetry(
        &telemetry_state,
        &engine_str_val,
        &voice,
        text.len(),
        synthesis_ms,
    );

    // Extract envelope
    let envelope = extract_envelope_or_default(&wav_bytes.bytes);

    // Save to history
    let history_path =
        save_to_history_storage(&config, &wav_bytes, &engine_id, &voice_name, &audio_ext);

    add_history_with_metadata(
        &history,
        &text,
        &engine_str_val,
        &voice,
        envelope.duration_ms,
        history_path,
        None,
        synthesis_ms,
        None,
    );
    let _ = app.emit("history-updated", ());

    // Show HUD with waveform visualization
    hud::show_hud(&app, envelope, Some(text.clone()));

    // Emit audio to frontend
    emit_audio_ready(&app, &wav_bytes, &text, abort_generation);

    log::info!("Re-spoke history entry: {}", entry_id);
    Ok(())
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use std::sync::mpsc;

    fn meta(sample_rate: u32) -> AudioFormatMeta {
        AudioFormatMeta {
            sample_rate,
            channels: 1,
            bits_per_sample: 16,
        }
    }

    fn stream_from_items(items: Vec<ChunkItem>, sample_rate: u32) -> ChunkStream {
        let (tx, rx) = mpsc::channel();
        for item in items {
            tx.send(item).unwrap();
        }
        drop(tx);
        ChunkStream::new(meta(sample_rate), rx)
    }

    #[test]
    fn multi_chunk_order_preserved_and_pcm_wrapped_into_valid_wav() {
        let a = vec![1u8, 2, 3, 4];
        let b = vec![5u8, 6, 7, 8];
        let stream = stream_from_items(
            vec![ChunkItem::Pcm(a.clone()), ChunkItem::Pcm(b.clone())],
            44100,
        );

        let mut seen: Vec<(usize, Vec<u8>, bool)> = Vec::new();
        let (wav, _ttfa) = drain_chunk_stream(stream, |idx, pcm, is_final, _| {
            seen.push((idx, pcm.to_vec(), is_final));
        })
        .expect("healthy stream drains");

        assert_eq!(seen.len(), 3, "two data chunks plus the terminal marker");
        assert_eq!(seen[0], (0, a.clone(), false));
        assert_eq!(seen[1], (1, b.clone(), false));
        assert!(seen[2].2, "terminal event must be marked final");
        assert!(seen[2].1.is_empty());

        // Wrapped WAV parses back to the concatenated PCM with the stream meta.
        let info = crate::audio::wav::parse_wav_header(&wav.bytes).expect("wrapped WAV valid");
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.data_size, 8);
        let expected = [a, b].concat();
        assert_eq!(
            &wav.bytes[info.data_offset..info.data_offset + info.data_size],
            &expected
        );
    }

    #[test]
    fn enabled_effect_forces_fragment_path_over_stream_chunks() {
        // Streaming on, warm streaming backend, no effect: live chunks.
        assert!(allows_streaming_playback(true, true, false));
        // Any kill switch routes the reading through audio-fragment-ready,
        // the only path where buildPlaybackUrl applies the effect.
        assert!(!allows_streaming_playback(false, true, false));
        assert!(!allows_streaming_playback(true, false, false));
        assert!(!allows_streaming_playback(true, true, true));
    }

    #[test]
    fn invalid_captions_do_not_interrupt_live_audio_or_survive_in_history() {
        let valid = CaptionAlignment {
            text: "Hi".into(),
            words: vec![crate::tts::captions::WordTiming {
                text_start: 0,
                text_end: 2,
                start_ms: 0.0,
                end_ms: 0.1,
            }],
        };
        let mut invalid = valid.clone();
        invalid.words[0].text_end = 9;
        let cleared = CaptionAlignment {
            text: valid.text.clone(),
            words: Vec::new(),
        };
        for rejection in [ChunkItem::Captions(invalid), ChunkItem::ClearCaptions] {
            let stream = stream_from_items(
                vec![
                    ChunkItem::Captions(valid.clone()),
                    ChunkItem::Pcm(vec![1, 2]),
                    rejection,
                    ChunkItem::Pcm(vec![3, 4]),
                ],
                8000,
            );
            let mut seen = Vec::new();
            let (speech, _) = drain_chunk_stream(stream, |idx, pcm, is_final, captions| {
                seen.push((idx, pcm.to_vec(), is_final, captions.cloned()));
            })
            .unwrap();
            assert_eq!(speech.bytes, pcm_to_wav(&[1, 2, 3, 4], &meta(8000)));
            assert_eq!(speech.captions, None);
            assert_eq!(
                seen,
                vec![
                    (0, vec![], false, Some(valid.clone())),
                    (0, vec![1, 2], false, None),
                    (1, vec![], false, Some(cleared.clone())),
                    (1, vec![3, 4], false, None),
                    (2, vec![], true, None),
                ]
            );
            // The existing frontend accepts this as a replacement snapshot;
            // absent metadata on ordinary PCM events is not a clear command.
            assert!(cleared.validate().is_ok());
        }
        let next = stream_from_items(
            vec![
                ChunkItem::Captions(valid.clone()),
                ChunkItem::Pcm(vec![5, 6]),
            ],
            8000,
        );
        assert_eq!(
            drain_chunk_stream(next, |_, _, _, _| {})
                .unwrap()
                .0
                .captions,
            Some(valid)
        );
    }

    #[test]
    fn failed_item_mid_stream_is_an_error() {
        let stream = stream_from_items(
            vec![
                ChunkItem::Pcm(vec![1, 2]),
                ChunkItem::Failed("upstream connection reset".into()),
            ],
            16000,
        );
        let result = drain_chunk_stream(stream, |_, _, _, _| {});
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("connection reset"));
    }

    #[test]
    fn empty_stream_is_an_error_not_silence() {
        let stream = stream_from_items(Vec::new(), 22050);
        assert!(drain_chunk_stream(stream, |_, _, _, _| {}).is_err());
    }

    #[test]
    fn terminal_marker_always_emitted_even_for_single_chunk() {
        let stream = stream_from_items(vec![ChunkItem::Pcm(vec![9, 9, 9])], 8000);
        let mut finals = 0;
        let (wav, _) = drain_chunk_stream(stream, |_, _, is_final, _| {
            if is_final {
                finals += 1;
            }
        })
        .unwrap();
        assert_eq!(finals, 1);
        assert_eq!(wav.bytes.len(), 44 + 3);
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use crate::history::HistoryEntry;

    fn saved(history: &Mutex<HistoryLog>, text: &str, voice: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "copyspeak-cache-test-{}-{}.wav",
            std::process::id(),
            history.lock().unwrap().entries().len()
        ));
        let meta = AudioFormatMeta {
            sample_rate: 8000,
            channels: 1,
            bits_per_sample: 16,
        };
        std::fs::write(&path, pcm_to_wav(&[0, 0, 1, 0], &meta)).unwrap();
        history.lock().unwrap().add(HistoryEntry {
            id: text.to_string(),
            timestamp: chrono::Utc::now(),
            text: text.to_string(),
            text_length: text.len() as u32,
            tts_engine: "edge".into(),
            voice: voice.into(),
            speed: 1.0,
            output_format: None,
            output_path: Some(path.to_string_lossy().into_owned()),
            duration_ms: 0,
            batch_id: None,
            app_name: None,
            source: None,
            filters_applied: Vec::new(),
            success: true,
            error_message: None,
            attempts: 1,
            tags: Vec::new(),
            metadata: HashMap::new(),
        });
        path
    }

    #[test]
    fn replays_a_reading_only_when_every_fragment_is_saved() {
        let pagination = crate::config::PaginationConfig {
            enabled: true,
            fragment_size: 30,
        };
        let text = "First sentence goes here. Second sentence goes here.";
        let fragments = pagination::paginate_text(text, &pagination);
        assert_eq!(fragments.len(), 2);
        let history = Mutex::new(HistoryLog::new());
        let first = saved(&history, &fragments[0].text, "ava");
        assert!(find_cached_reading(&history, text, &pagination, "edge", "ava").is_none());

        let second = saved(&history, &fragments[1].text, "ava");
        let joined = find_cached_reading(&history, text, &pagination, "edge", "ava").unwrap();
        assert_eq!(
            crate::audio::wav::parse_wav_header(&joined.bytes)
                .unwrap()
                .data_size,
            8,
            "both fragments' samples, in order"
        );
        assert!(find_cached_reading(&history, text, &pagination, "edge", "other").is_none());

        std::fs::remove_file(&second).unwrap();
        assert!(find_cached_reading(&history, text, &pagination, "edge", "ava").is_none());
        std::fs::remove_file(&first).unwrap();
    }
}

#[cfg(test)]
mod retrigger_tests {
    use super::*;

    #[test]
    fn only_interrupt_mode_aborts_the_running_reading() {
        assert!(retrigger_interrupts(&config::RetriggerMode::Interrupt));
        assert!(!retrigger_interrupts(&config::RetriggerMode::Queue));
    }

    #[test]
    fn an_unreplaced_reading_publishes_its_result() {
        assert!(reading_may_publish(7, 7));
    }

    /// The regression this fixes: a retrigger bumps the generation, so the
    /// replaced reading must go quiet instead of caching its cancelled audio
    /// and telling the frontend the replacement had already finished.
    #[test]
    fn a_replaced_reading_does_not_publish() {
        let replaced = 7u32;
        let replacement = replaced.wrapping_add(1);
        assert!(!reading_may_publish(replaced, replacement));
    }

    /// A user Stop also advances the generation, so a reading stopped in the
    /// tail window (after its last fragment, during the cache concat) must not
    /// publish either. This is the second, post-concat gate.
    #[test]
    fn a_stopped_reading_does_not_publish_after_the_concat() {
        let stopped_reading = 11u32;
        assert!(!reading_may_publish(stopped_reading, stopped_reading + 1));
    }

    /// The emitters gate on the *generation*, not the flag: a replacement clears
    /// `ABORT_REQUESTED` as it starts, so a flag check would let a replaced
    /// reading's in-flight fragment through into the new reading.
    #[test]
    fn emit_gate_rejects_a_replaced_reading_even_after_the_flag_is_cleared() {
        let replaced = 5u32;
        let replacement = 6u32;
        // `ABORT_REQUESTED` would already be false here (the replacement cleared
        // it); only the generation still distinguishes the two readings.
        assert!(!reading_may_publish(replaced, replacement));
        assert!(reading_may_publish(replacement, replacement));
    }

    /// The per-fragment history/file write in `speak_queued` uses this same
    /// predicate, so a fragment that finished synthesizing after the reading was
    /// replaced must not leave a history row or caption sidecar behind.
    #[test]
    fn a_replaced_reading_does_not_persist_its_finished_fragments() {
        let replaced = 42u32;
        assert!(!reading_may_publish(replaced, replaced.wrapping_add(1)));
        assert!(reading_may_publish(replaced, replaced));
    }

    #[test]
    fn a_generation_wrap_still_separates_two_readings() {
        // `do_abort_synthesis` wraps with `wrapping_add`, so the successor of
        // u32::MAX is 0 and must still read as a different reading.
        assert!(!reading_may_publish(u32::MAX, 0));
    }
}
