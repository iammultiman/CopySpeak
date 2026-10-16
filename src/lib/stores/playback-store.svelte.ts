/**
 * Global Playback Store
 *
 * Single source of truth for audio playback state across all routes.
 * The GlobalPlayer component mounts the <audio> element and calls
 * setAudioElement() + setupListeners() on mount.
 *
 * Supports streaming pagination playback: fragments are queued and played
 * sequentially as they arrive from the backend.
 */

import { isTauri } from "$lib/services/tauri.js";
import { invoke } from "@tauri-apps/api/core";
import type { AppConfig, EffectId } from "$lib/types";
import { applyFadeIn, audioBufferToWavBlob, detectAudioMimeType } from "./playback/audio-utils.js";
import { AudioAnalyser, type EmitPayload } from "./playback/analyser.js";
import { PcmStreamScheduler, type StreamChunkPayload } from "./playback/pcm-stream.js";
import { stretchBuffer } from "./playback/time-stretch.js";
import { getEffect } from "./playback/effects/registry.js";
import { FragmentQueue, type QueuedFragment } from "./playback/fragment-queue.js";
import { hudStore } from "./hud-store.svelte.js";
import { validCaptionAlignment, type CaptionAlignment } from "$lib/models/captions.js";
import type { HudCaptionPayload } from "$lib/types/hud.js";
import { toast } from "svelte-sonner";

class PlaybackStore {
  isPlaying = $state(false);
  isPaused = $state(false);
  isSynthesizing = $state(false);
  isLoadingAudio = $state(false);
  error = $state<string | null>(null);
  hasCachedAudio = $state(false);
  // Retained after stop/completion so the owning history row can offer Replay.
  historyReadingId = $state<string | null>(null);
  // Audible fragment's text, caption timings and clock; null when nothing plays.
  caption = $state.raw<HudCaptionPayload | null>(null);

  // Pagination state for HUD display
  currentFragmentIndex = $state<number | null>(null);
  totalFragments = $state<number | null>(null);

  // Synced from config by whoever has it loaded (synthesize-page or global-player)
  pitch = $state(1.0);
  volume = $state(100);
  speed = $state(1.0);
  activeEffect = $state<EffectId>("none");

  private _audioEl: HTMLAudioElement | null = null;
  private _audioCtx: AudioContext | null = null;
  private _decodedBuffer: AudioBuffer | null = null;
  private _originalBytes: ArrayBuffer | null = null;
  private _cachedPitchUrl: { ratio: number; effectId: EffectId; url: string } | null = null;
  private _unlistenFns: Array<() => void> = [];
  private _emit: ((name: string, payload: EmitPayload) => Promise<void>) | null = null;
  private _emitTo: ((target: string, name: string, payload: EmitPayload) => Promise<void>) | null =
    null;
  private _stopping = false;
  private _playbackGeneration = 0;
  private _readingText = "";
  private _readingFailure: string | null = null;
  private _fragmentCaptions: CaptionAlignment | null = null;
  private _streamStopped = false;
  // Last browser-bridge progress report (ms clock + wall time). The caption
  // clock ticks at 40 Hz; round-tripping every tick to Rust for browser
  // readings flooded IPC and stuttered playback, so reports are throttled.
  private _lastBrowserReportMs = -1;
  private _lastBrowserReportAt = 0;
  private _fragmentText = "";
  private _captionTimer: ReturnType<typeof setInterval> | null = null;
  private _lastCaption: HudCaptionPayload | null = null;
  private _streamCaptions = new Map<
    string,
    { text: string; durationMs: number; captions?: CaptionAlignment | null }
  >();

  // Modular components
  private _analyser = new AudioAnalyser();
  private _fragmentQueue: FragmentQueue;
  private _pcmScheduler: PcmStreamScheduler | null = null;

  // Browser companion: while a browser-initiated reading is active, the
  // audible-clock position is reported to the Rust bridge, which projects the
  // current word into the page selection (see src-tauri/src/browser_bridge.rs).
  // The id identifies which session each report belongs to, so a late report or
  // terminal event from a superseded reading cannot end or move its replacement.
  private _browserReadingActive = false;
  private _browserReadingId: string | null = null;
  private _playingBrowserReadingId: string | null = null;
  /**
   * Whether the tracked browser reading has produced audio yet.
   *
   * A retrigger in Interrupt mode aborts the *previous* playback to start this
   * one, so that abort's `synthesis-state-change: false`, `playback-stop` and
   * `synthesis-aborted` arrive while this reading is active but silent. Those
   * events belong to the reading that was replaced and must not tear this one
   * down before it has spoken.
   */
  private _browserReadingStarted = false;

  constructor() {
    // Initialize fragment queue with handlers
    this._fragmentQueue = new FragmentQueue({
      onFragmentPlay: async (fragment: QueuedFragment) => {
        console.log(
          "[PlaybackStore] onFragmentPlay: index",
          fragment.index,
          "total",
          fragment.total
        );
        this.currentFragmentIndex = fragment.index;
        this.totalFragments = fragment.total;
        this._fragmentText = fragment.text;
        this._fragmentCaptions = validCaptionAlignment(fragment.captions)
          ? fragment.captions
          : null;
        await this.handleAudioReady(fragment.audioBase64, fragment.browserReadingId);
      },
      onQueueComplete: () => {
        console.log("[PlaybackStore] onQueueComplete");
        this.finishPlayback();
      }
    });
  }

  setAudioElement(el: HTMLAudioElement | null) {
    this._audioEl = el;
    if (el) {
      el.onplay = () => {
        this.isPlaying = true;
        this.isPaused = false;
        this.startCaptionClock();
      };
      el.onpause = () => {
        if (this._stopping) return;
        this.isPaused = !el.ended;
        this.isPlaying = !el.ended;
      };
      el.onended = () => {
        this._fragmentQueue.handleFragmentEnded();
      };
    }
  }

  private startCaptionClock() {
    if (this._captionTimer === null) {
      this._captionTimer = setInterval(() => this.publishCaption(), 25);
    }
    this.publishCaption();
  }

  private stopCaptionClock() {
    if (this._captionTimer !== null) clearInterval(this._captionTimer);
    this._captionTimer = null;
    this._lastCaption = null;
    this.caption = null;
    this._streamCaptions.clear();
  }

  private publishCaption() {
    let caption: HudCaptionPayload | null = null;
    let audibleBrowserReadingId = this._playingBrowserReadingId;
    if (this._pcmScheduler) {
      const position = this._pcmScheduler.getPlaybackPosition();
      // The audible source owns progress and completion, not the most recently
      // arrived chunk: browser PCM can buffer behind desktop PCM in the same
      // scheduler, and reporting arrival as playback would end the browser
      // session while its audio is still queued.
      if (position) {
        audibleBrowserReadingId = position.browserReadingId ?? null;
        this._playingBrowserReadingId = audibleBrowserReadingId;
        if (this._browserReadingActive && audibleBrowserReadingId === this._browserReadingId) {
          this._browserReadingStarted = true;
        }
      } else {
        audibleBrowserReadingId = this._playingBrowserReadingId;
      }
      const fragment = position
        ? this._streamCaptions.get(
            JSON.stringify([position.browserReadingId ?? null, position.fragmentIndex])
          )
        : null;
      if (position && fragment) {
        caption = {
          text: fragment.captions?.text ?? fragment.text,
          captions: fragment.captions,
          position_ms: position.positionMs,
          duration_ms: fragment.durationMs,
          paused: this.isPaused,
          active: true
        };
      } else if (this._lastCaption) {
        caption = { ...this._lastCaption, active: false, paused: this.isPaused };
      }
    } else if (this._audioEl && this.isPlaying) {
      const el = this._audioEl;
      const duration = Number.isFinite(el.duration)
        ? el.duration
        : (this._decodedBuffer?.duration ?? 0);
      // The element plays through the AudioContext (analyser), and its media clock
      // leads the speakers by the context's output latency, in wall seconds.
      const latency = Math.max(0, this._audioCtx?.outputLatency || 0) * el.playbackRate;
      caption = {
        text: this._fragmentCaptions?.text ?? (this._fragmentText || this._readingText),
        captions: this._fragmentCaptions,
        position_ms: Math.max(0, el.currentTime - latency) * 1000,
        duration_ms: duration * 1000,
        paused: this.isPaused,
        active: !this.isLoadingAudio && !el.ended && !el.seeking && el.readyState >= 2
      };
    }
    if (!caption) return;
    const previousPaused = this._lastCaption?.paused ?? false;
    this._lastCaption = caption;
    this.caption = caption;
    hudStore.handleCaption(caption);
    void this._emitTo?.("hud", "hud:caption", caption);
    // Report the audible clock to the browser bridge. Best-effort: the bridge
    // may not exist (older app build) or no reading may be active. Skip unless
    // the clock advanced enough or a pause state changed, so a 40 Hz caption
    // clock does not become a 40 Hz IPC round-trip.
    if (this._browserReadingActive && audibleBrowserReadingId === this._browserReadingId) {
      const now = Date.now();
      const positionMs = Math.round(caption.position_ms);
      const pauseChanged = previousPaused !== caption.paused;
      const advancedEnough = Math.abs(positionMs - this._lastBrowserReportMs) >= 100;
      if (advancedEnough || pauseChanged || now - this._lastBrowserReportAt >= 500) {
        const fragmentIndex = this._pcmScheduler
          ? (this._pcmScheduler.getPlaybackPosition()?.fragmentIndex ??
            this.currentFragmentIndex ??
            0)
          : (this.currentFragmentIndex ?? 0);
        this._lastBrowserReportMs = positionMs;
        this._lastBrowserReportAt = now;
        void invoke("browser_reading_progress", {
          readingId: this._browserReadingId,
          fragmentIndex,
          positionMs,
          paused: caption.paused
        }).catch(() => {});
      }
    }
  }

  async buildPlaybackUrl(pitchRatio: number): Promise<string> {
    const generation = this._playbackGeneration;
    const effectId = this.activeEffect;
    if (
      this._cachedPitchUrl &&
      this._cachedPitchUrl.ratio === pitchRatio &&
      this._cachedPitchUrl.effectId === effectId
    ) {
      return this._cachedPitchUrl.url;
    }
    if (this._cachedPitchUrl) {
      URL.revokeObjectURL(this._cachedPitchUrl.url);
      this._cachedPitchUrl = null;
    }
    const effect = getEffect(effectId);
    let blob: Blob;
    if (pitchRatio === 1.0 && !effect && this._originalBytes) {
      const mimeType = detectAudioMimeType(this._originalBytes);
      blob = new Blob([this._originalBytes], { type: mimeType });
    } else if (this._decodedBuffer && this._audioCtx) {
      let buffer: AudioBuffer;
      if (pitchRatio === 1.0) {
        // Copy to avoid mutating cached decoded buffer when applying fade-in
        buffer = new AudioBuffer({
          length: this._decodedBuffer.length,
          numberOfChannels: this._decodedBuffer.numberOfChannels,
          sampleRate: this._decodedBuffer.sampleRate
        });
        for (let c = 0; c < this._decodedBuffer.numberOfChannels; c++) {
          buffer.copyToChannel(this._decodedBuffer.getChannelData(c), c);
        }
      } else {
        // Pitch shift only - duration stays native so caption positions map 1:1.
        buffer = stretchBuffer(this._decodedBuffer, 1, pitchRatio);
      }
      if (effect) {
        buffer = await effect.process(buffer, this._audioCtx);
      }
      applyFadeIn(buffer, 10);
      blob = audioBufferToWavBlob(buffer);
    } else {
      return "";
    }
    if (generation !== this._playbackGeneration) return "";
    const url = URL.createObjectURL(blob);
    this._cachedPitchUrl = { ratio: pitchRatio, effectId, url };
    return url;
  }

  async handleAudioReady(base64: string, browserReadingId?: string | null): Promise<void> {
    const generation = this._playbackGeneration;
    this.isLoadingAudio = true;
    this.error = this._readingFailure;
    this.hasCachedAudio = false;
    this.markBrowserStarted(browserReadingId);
    try {
      console.log("[PlaybackStore] handleAudioReady called, base64 length:", base64.length);
      const binary = atob(base64);
      const arrayBuffer = new ArrayBuffer(binary.length);
      const bytes = new Uint8Array(arrayBuffer);
      for (let i = 0; i < binary.length; i++) {
        bytes[i] = binary.charCodeAt(i);
      }

      this._originalBytes = arrayBuffer.slice(0);
      if (this._cachedPitchUrl) {
        URL.revokeObjectURL(this._cachedPitchUrl.url);
        this._cachedPitchUrl = null;
      }

      if (!this._audioCtx) {
        this._audioCtx = new AudioContext();
      }

      // Resume AudioContext if suspended (required on clean Windows 11 / strict autoplay policies)
      if (this._audioCtx.state === "suspended") {
        await this._audioCtx.resume();
      }
      if (generation !== this._playbackGeneration) return;

      // Wire AnalyserNode once per audio element (guard prevents double-wiring)
      if (this._audioEl && !this._analyser.getAnalyser()) {
        this._analyser.setup(this._audioEl, this._audioCtx, {
          emitTo: this._emitTo
        });
      }

      const decoded = await this._audioCtx.decodeAudioData(arrayBuffer.slice(0));
      if (generation !== this._playbackGeneration) return;
      this._decodedBuffer = decoded;
      if (this._decodedBuffer) {
        const accurateDurationMs = Math.round(this._decodedBuffer.duration * 1000);
        hudStore.setAccurateDurationMs(accurateDurationMs);
        // Emit to HUD window for cross-window state sync
        void this._emit?.("hud:audio-duration", accurateDurationMs);
      }
      const url = await this.buildPlaybackUrl(this.pitch);
      if (generation !== this._playbackGeneration) return;
      if (!this._audioEl || !url) throw new Error("Audio player is not ready");
      this._audioEl.src = url;
      this._analyser.start(); // Start amplitude capture BEFORE audio plays
      await this.playAudio();
      if (generation !== this._playbackGeneration) return;
      this.hasCachedAudio = true;
    } catch (e) {
      if (generation !== this._playbackGeneration) return;
      this.handleStop();
      this.error = `Audio playback failed: ${e instanceof Error ? e.message : String(e)}`;
    } finally {
      if (generation === this._playbackGeneration) this.isLoadingAudio = false;
    }
  }

  /**
   * Handle incoming audio fragment from backend.
   * Queues the fragment and starts processing if not already.
   */
  async handleFragmentReady(payload: {
    browser_reading_id?: string | null;
    audio_base64: string;
    fragment_index: number;
    fragment_total: number;
    is_final: boolean;
    text: string;
    captions?: CaptionAlignment | null;
  }): Promise<void> {
    console.log(
      "[PlaybackStore] handleFragmentReady: index",
      payload.fragment_index,
      "total",
      payload.fragment_total,
      "is_final",
      payload.is_final
    );
    // Add to queue
    this._fragmentQueue.enqueue({
      browserReadingId: payload.browser_reading_id,
      audioBase64: payload.audio_base64,
      index: payload.fragment_index,
      total: payload.fragment_total,
      text: payload.text,
      captions: payload.captions,
      isFinal: payload.is_final
    });
    console.log(
      "[PlaybackStore] Queue length:",
      this._fragmentQueue.getQueueLength(),
      "isProcessing:",
      this._fragmentQueue.isProcessing()
    ); // Start processing if not already
    if (!this._fragmentQueue.isProcessing()) {
      await this._fragmentQueue.startProcessing();
    }
  }

  /**
   * Note that the tracked browser reading has produced audio.
   *
   * Learned from the audio handlers themselves, so it is known synchronously —
   * before any stop or completion event for this reading.
   */
  private markBrowserStarted(browserReadingId?: string | null): void {
    this._playingBrowserReadingId = browserReadingId ?? null;
    if (this._browserReadingActive && browserReadingId === this._browserReadingId) {
      this._browserReadingStarted = true;
    }
  }

  /**
   * Whether a reading-lifecycle event belongs to the reading this store is
   * running, rather than to a reading it replaced.
   *
   * A browser reading that has produced no audio yet is still in its replaced
   * reading's teardown window, so a completion, stop or abort arriving now is
   * that teardown's. A recorded failure is this reading's own: it is the reason
   * a silent reading has nothing more to say, and its report must not be
   * suppressed. A browser reading that has produced no audio and has no failure
   * is left running; the bridge reaps it only if the frontend stops reporting.
   */
  private isCurrentReadingEvent(): boolean {
    if (!this._browserReadingActive) return true;
    return this._browserReadingStarted || this._readingFailure !== null;
  }

  /**
   * Release the active browser reading to the bridge exactly once.
   *
   * `stopped` distinguishes a user/hotkey Stop from a reading that drained
   * normally; a recorded synthesis failure wins over both, so a failed section
   * is never reported as a clean completion.
   *
   * Only the audio belonging to this browser reading sends a report. The bridge decides whether a `completed` or
   * `cancelled` may end the session, because it is the authority on whether this
   * reading actually produced audio: a silent reading was replaced, so the
   * completion came from the reading it replaced. A user Stop reaches the bridge
   * through the stop commands instead, so nothing here has to guess.
   */
  private reportBrowserEnd(stopped: boolean): void {
    if (!this._browserReadingActive) return;
    if (this._playingBrowserReadingId !== this._browserReadingId) return;
    const readingId = this._browserReadingId;
    this._browserReadingActive = false;
    this._browserReadingId = null;
    this._browserReadingStarted = false;
    const status = this._readingFailure ? "error" : stopped ? "cancelled" : "completed";
    void invoke("browser_reading_finished", { readingId, status }).catch(() => {});
  }

  /**
   * Shared queue-completion path for batch fragments and streamed PCM chunks:
   * stops the analyser, resets playback state, hides the HUD.
   */
  private finishPlayback(): void {
    this.stopCaptionClock();
    this._analyser.stop();
    this.isPlaying = false;
    this.isPaused = false;
    this.currentFragmentIndex = null;
    this.totalFragments = null;
    this._pcmScheduler = null;
    this.reportBrowserEnd(false);
    this._playingBrowserReadingId = null;
    void this._emit?.("hud:stop", null);
  }

  /**
   * Handle one PCM chunk of streaming synthesis audio. Lazily creates the
   * scheduler (which prebuffers ~250ms before starting gap-free playback) and
   * feeds it the chunk.
   */
  handleStreamChunk(payload: StreamChunkPayload): void {
    if (this._streamStopped) return;
    if (payload.audio_base64) this.markBrowserStarted(payload.browser_reading_id);
    if (!this._audioCtx) {
      this._audioCtx = new AudioContext();
    }
    const ctx = this._audioCtx;
    // Resume AudioContext if suspended (autoplay policies, or a paused stream)
    if (ctx.state === "suspended" && !this.isPaused) {
      void ctx.resume();
    }
    if (!this._pcmScheduler) {
      console.log("[PlaybackStore] creating PCM scheduler for fragment", payload.fragment_index);
      this._pcmScheduler = new PcmStreamScheduler({
        ctx,
        destination: ctx.destination,
        onComplete: () => {
          console.log("[PlaybackStore] stream playback complete");
          this.finishPlayback();
        }
      });
      this._pcmScheduler.setVolume(this.volume);
      this._pcmScheduler.setRate(this.speed, this.pitch);
      this.currentFragmentIndex = payload.fragment_index;
      this.totalFragments = payload.fragment_total;
      this.isPlaying = true;
      this.isPaused = false;
    }
    const captionKey = JSON.stringify([payload.browser_reading_id ?? null, payload.fragment_index]);
    if (payload.text !== undefined) {
      this._streamCaptions.set(captionKey, {
        text: payload.text,
        durationMs: 0,
        captions: this._streamCaptions.get(captionKey)?.captions
      });
    }
    const caption = this._streamCaptions.get(captionKey);
    if (caption && validCaptionAlignment(payload.captions)) caption.captions = payload.captions;
    if (caption && payload.fragment_duration_ms !== undefined) {
      caption.durationMs = payload.fragment_duration_ms;
    }
    this._pcmScheduler.handleChunk(payload);
    if (this._pcmScheduler) this.startCaptionClock();
  }

  async playAudio() {
    if (!this._audioEl) {
      throw new Error("Audio player is not ready");
    }
    this._audioEl.volume = this.volume / 100;
    // Time-stretch rather than resample, so speed never shifts pitch.
    this._audioEl.preservesPitch = true;
    this._audioEl.playbackRate = this.speed;
    console.log(
      "[PlaybackStore] playAudio: volume",
      this.volume,
      "speed",
      this.speed,
      "src",
      this._audioEl.src?.substring(0, 50)
    );
    await this._audioEl.play();
  }

  async handleReplay(): Promise<void> {
    this.historyReadingId = null;
    // A paginated reading is emitted fragment-by-fragment and never assembled in
    // the frontend, so `_originalBytes` only holds the last fragment. The whole
    // reading exists only in the backend cache — ask it to replay that.
    if (isTauri) {
      try {
        await invoke("replay_cached");
        return;
      } catch (e) {
        this.error = `Audio replay failed: ${e instanceof Error ? e.message : String(e)}`;
        return;
      }
    }
    if (!this._audioEl) return;
    const url = await this.buildPlaybackUrl(this.pitch);
    if (url) {
      this._audioEl.src = url;
      this._audioEl.currentTime = 0;
      try {
        await this.playAudio();
      } catch (e) {
        this.handleStop();
        this.error = `Audio playback failed: ${e instanceof Error ? e.message : String(e)}`;
      }
    }
  }

  handleStop() {
    this._streamStopped = true;
    this.stopCaptionClock();
    this._playbackGeneration += 1;
    this.isLoadingAudio = false;
    this._analyser.stop();
    this._stopping = true;

    // Clear the fragment queue and any active PCM stream
    this._fragmentQueue.clear();
    this._pcmScheduler?.stop();
    this._pcmScheduler = null;
    this.currentFragmentIndex = null;
    this.totalFragments = null;

    if (this._audioEl) {
      this._audioEl.pause();
      this._audioEl.currentTime = 0;
    }
    this.isPlaying = false;
    this.isPaused = false;
    // A Stop also ends any browser reading: a failure after audio was emitted
    // leaves the session waiting for this report, and without it the session
    // would stay registered (and could answer a later reading's progress and
    // finish calls) until the bridge's liveness lease reaped it.
    this.reportBrowserEnd(true);
    this._playingBrowserReadingId = null;
    this._readingFailure = null;
    void this._emit?.("hud:stop", null);
    setTimeout(() => {
      this._stopping = false;
    }, 0);
  }

  handleTogglePause() {
    if (this._pcmScheduler) {
      if (this.isPaused) {
        this._pcmScheduler.resume();
        this.isPaused = false;
      } else {
        this._pcmScheduler.pause();
        this.isPaused = true;
      }
      return;
    }
    if (!this._audioEl) return;
    if (this._audioEl.paused) {
      this._audioEl.play().catch(() => {});
      this.isPaused = false;
    } else {
      this._audioEl.pause();
      this.isPaused = true;
    }
  }

  // Keep volume/speed in sync with config (called by synthesize-page via $effect)
  syncPlaybackConfig(volume: number, speed: number, pitch: number, effect: EffectId = "none") {
    this.volume = volume;
    this.speed = speed;
    this.pitch = pitch;
    if (this.activeEffect !== effect) {
      this.activeEffect = effect;
      if (this._cachedPitchUrl) {
        URL.revokeObjectURL(this._cachedPitchUrl.url);
        this._cachedPitchUrl = null;
      }
    }
    if (this._audioEl) {
      this._audioEl.volume = volume / 100;
      this._audioEl.preservesPitch = true;
      this._audioEl.playbackRate = speed;
    }
    // Keep the streaming scheduler in sync while it owns playback
    if (this._pcmScheduler?.isActive()) {
      this._pcmScheduler.setVolume(volume);
      this._pcmScheduler.setRate(speed, pitch);
    }
    // Sync pitch and speed to HUD store for progress bar timing
    hudStore.setPitch(pitch);
    hudStore.setSpeed(speed);
  }

  async setupListeners(): Promise<void> {
    if (!isTauri) return;
    console.log("[PlaybackStore] Setting up listeners...");
    try {
      const { listen, emit, emitTo } = await import("@tauri-apps/api/event");
      this._emit = emit;
      this._emitTo = emitTo;
      // Legacy audio-ready carries only bytes. HUD metadata supplies its text;
      // queued/streamed fragments carry their own text and take precedence.
      const captionListeners = await Promise.all(
        ["hud:start", "hud:playback_start", "hud:synthesizing"].map((name) =>
          listen<{ text: string | null }>(name, (event) => {
            this._readingText = event.payload.text ?? "";
          })
        )
      );
      console.log("[PlaybackStore] event API loaded");

      // Note: AnalyserNode is set up in handleAudioReady once we have an AudioContext
      // _audioCtx is null here until audio is first decoded

      // Legacy single audio-ready event (for non-paginated playback)
      const unAudioReady = await listen<string>("audio-ready", async (e) => {
        console.log("[PlaybackStore] audio-ready received");
        this._fragmentQueue.enqueue({
          audioBase64: e.payload,
          index: 1,
          total: 1,
          text: "",
          isFinal: true
        });
        if (!this._fragmentQueue.isProcessing()) {
          await this._fragmentQueue.startProcessing();
        }
      });

      // New streaming fragment-ready event
      const unFragmentReady = await listen<{
        browser_reading_id?: string | null;
        audio_base64: string;
        fragment_index: number;
        fragment_total: number;
        is_final: boolean;
        text: string;
        captions?: CaptionAlignment | null;
      }>("audio-fragment-ready", async (e) => {
        console.log(
          "[PlaybackStore] audio-fragment-ready received, index:",
          e.payload.fragment_index,
          "total:",
          e.payload.fragment_total
        );
        await this.handleFragmentReady(e.payload);
      });

      // Streaming PCM chunks from streaming-capable backends (ElevenLabs)
      const unStreamChunk = await listen<StreamChunkPayload>("audio-stream-chunk", (e) => {
        this.handleStreamChunk(e.payload);
      });

      // Authoritative end-of-synthesis signal. Route it to whichever player
      // actually owns playback: PCM streams complete via their scheduler, the
      // fragment queue completes when its final fragment's audio ends. Signalling
      // both would let the idle one finish playback early (the fragment queue is
      // always empty during PCM streaming), which clears the scheduler mid-drain.
      const unPaginationComplete = await listen("pagination:complete", () => {
        if (this._pcmScheduler) {
          this._pcmScheduler.markQueueComplete();
        } else {
          this._fragmentQueue.markComplete();
        }
      });

      // A reading that ended without reaching its last fragment (mid-reading
      // stop/abort) must not leave the fragment queue waiting forever.
      const unPaginationStopped = await listen("pagination:stopped", () => {
        if (this._pcmScheduler) {
          this._pcmScheduler.markQueueComplete();
        } else {
          this._fragmentQueue.markComplete();
        }
      });

      console.log("[PlaybackStore] All listeners registered");

      const unPlaybackStop = await listen("playback-stop", () => {
        // Like `synthesis-aborted` above: a silent browser reading must not be
        // stopped by the teardown of the reading it replaced.
        if (this.isCurrentReadingEvent()) this.handleStop();
      });

      const unTogglePause = await listen("playback-toggle-pause", () => {
        this.handleTogglePause();
      });

      // Browser companion: the Rust bridge tells us a browser-initiated
      // reading started/ended so we only report positions while relevant.
      const unBrowserReading = await listen<{ active: boolean; reading_id?: string }>(
        "browser-reading",
        (e) => {
          if (e.payload.active === true) {
            this._browserReadingActive = true;
            this._browserReadingId = e.payload.reading_id ?? null;
            this._browserReadingStarted = false;
            return;
          }
          // This store tracks one reading. A terminal event ends it when it
          // belongs to that reading; an unscoped one is treated as belonging to
          // it (an older app build sent no id, and only one session could exist
          // then). A different id is a superseded reading's, and must not end
          // the live one.
          const eventId = e.payload.reading_id;
          if (eventId === undefined || eventId === null || eventId === this._browserReadingId) {
            this._browserReadingActive = false;
            this._browserReadingId = null;
            this._browserReadingStarted = false;
          }
        }
      );

      // Effects/speed/pitch chosen outside the Play page (Effects page, profile
      // switch) only ever reach the backend config; re-sync here so the next
      // reading uses them instead of the values this store held at startup.
      const unConfigChanged = await listen("config-changed", async () => {
        try {
          const config = await invoke<AppConfig>("get_config");
          const { volume } = config.playback;
          const profile = config.tts.profiles.find((p) => p.id === config.tts.active_profile_id);
          const effect = profile?.effects?.enabled ? profile.effects.active_effect : "none";
          this.syncPlaybackConfig(volume, profile?.speed ?? 1.0, profile?.pitch ?? 1.0, effect);
        } catch (e) {
          console.error("[PlaybackStore] Failed to re-sync after config-changed:", e);
        }
      });

      const unReadingFailed = await listen<string>("reading-failed", (event) => {
        this._readingFailure = event.payload;
        this.error = event.payload;
        toast.error(event.payload, { duration: 15000 });
      });

      const unTruncated = await listen<{
        original_length: number;
        truncated_length: number;
        max_length: number;
      }>("text-truncated", (event) => {
        toast.warning(
          `Copied text was shortened from ${event.payload.original_length} to ${event.payload.truncated_length} characters by the reading limit (${event.payload.max_length}).`,
          { duration: 15000 }
        );
      });

      const unSynthesis = await listen<boolean>("synthesis-state-change", (e) => {
        if (e.payload) {
          this._readingFailure = null;
          this.error = null;
          this.historyReadingId = null;
          this._streamStopped = false;
          this._pcmScheduler?.beginReading();
        } else {
          // Synthesis ended. If it failed mid-reading, no final fragment (or
          // pagination:stopped) will ever arrive, so release a starved queue.
          // A no-op when the final fragment is still queued, so a normal reading
          // is never cut short.
          //
          // A silent browser reading must not act on this: the end belongs to
          // the reading it replaced, and completing an empty queue here would
          // run `finishPlayback` and tear the new reading down before it spoke.
          if (this.isCurrentReadingEvent()) {
            if (this._pcmScheduler) this._pcmScheduler.markQueueComplete();
            else this._fragmentQueue.markComplete();
          }
        }
        this.isSynthesizing = e.payload;
      });

      const unAbort = await listen("synthesis-aborted", () => {
        // A silent browser reading must not be stopped by the abort of the
        // reading it replaced (see `isCurrentReadingEvent`).
        if (this.isCurrentReadingEvent()) this.handleStop();
      });

      this._unlistenFns = [
        ...captionListeners,
        unAudioReady,
        unFragmentReady,
        unStreamChunk,
        unPaginationComplete,
        unPaginationStopped,
        unPlaybackStop,
        unTogglePause,
        unBrowserReading,
        unConfigChanged,
        unReadingFailed,
        unTruncated,
        unSynthesis,
        unAbort
      ];
    } catch (e) {
      console.error("Failed to setup playback listeners:", e);
    }
  }

  teardownListeners() {
    this._readingFailure = null;
    this.handleStop();
    this._analyser.stop();
    this._analyser.destroy();
    this._pcmScheduler?.stop();
    this._pcmScheduler = null;
    for (const fn of this._unlistenFns) fn();
    this._unlistenFns = [];
    if (this._cachedPitchUrl) {
      URL.revokeObjectURL(this._cachedPitchUrl.url);
      this._cachedPitchUrl = null;
    }
    if (this._audioCtx) {
      void this._audioCtx.close();
      this._audioCtx = null;
    }
    this._emit = null;
    this._emitTo = null;
    this._decodedBuffer = null;
    this._originalBytes = null;
    this.hasCachedAudio = false;
    this._fragmentQueue.clear();
    this.currentFragmentIndex = null;
    this.totalFragments = null;
  }
}

export const playbackStore = new PlaybackStore();
