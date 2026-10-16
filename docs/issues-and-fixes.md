# X-CopySpeak: Reported Issues, Findings, and Fixes

Last updated: 2026-10-16. Current fork build: **X-CopySpeak 0.2.14**.

## Fork Improvement Summary

X-CopySpeak is the fork maintained at
[iammultiman/X-CopySpeak](https://github.com/iammultiman/X-CopySpeak), based on
[ilyaizen/CopySpeak](https://github.com/ilyaizen/CopySpeak). This ledger records
the implementation and review history since the available 0.2.8 source baseline;
it is not a claim that inherited engines, Linux support, profiles, captions,
effects, or the browser companion were invented in this fork.

| Area                                      | Changes since the baseline                                                                                                                                                 | Evidence and current qualification                                                                                                  |
| ----------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Original playback complaints              | Earlier paginated playback, configured fragment sizes, word-safe splitting, fewer browser IPC updates, and removal of duplicate shortcut registration                      | Pagination user-confirmed on 2026-10-09; latency and glitching appeared resolved in those tests, not benchmarked on every engine    |
| Pause, Stop and Replay                    | Pause/Resume controls, single toggle ownership, shared Stop/Abort cancellation, queue reset and whole-reading Replay assembly                                              | Pause/abort/resume user-confirmed in tested scenarios; later lifecycle and Replay corrections covered by automated checks           |
| Taskbar identity                          | Explicit Windows AppUserModelID, preserving installer identity                                                                                                             | Icon issue appeared resolved in user tests; stale Windows pins and upgrade paths remain separate concerns                           |
| Retrigger settings                        | Correct `interrupt`/`queue` contract, then cancellation-generation handling across reading triggers and concurrent control-server requests                                 | Setting save user-confirmed; broader implementation shipped in 0.2.12 and needs installed scenario coverage                         |
| Local startup and cancellation            | Background prewarm after profile/settings changes, Kokoro native-window delivery, effects-disabled bundled Kokoro profile, cancellable daemon waits and batch collection   | Implemented in 0.2.13; hardware-specific first-audio timings remain unmeasured                                                      |
| Engine failures                           | Legacy Piper profile migration and path repair, Gemini read-aloud request framing/model migration, Edge alternate-host fallback                                            | Implemented in 0.2.13; Edge service recovery was user-confirmed separately and does not validate the fallback on the user's network |
| Incomplete articles                       | Bounded transient retries, visible section-failure/truncation notices, preserved transport-error causes, no retry after emitted PCM                                        | Implemented and checked in 0.2.14; this reports unrecovered failures honestly rather than promising every passage will complete     |
| Browser lifecycle and Queue After Current | Exact reading IDs on activation/delivery/progress/finish, deferred teardown while buffered audio drains, explicit Stop handling, batch/PCM ownership and caption isolation | Implemented and checked in 0.2.14; installed Windows behavior is not yet user-confirmed                                             |
| Fork distribution                         | Visible X-CopySpeak branding, eight synchronized version locations, unsigned Windows installer workflow with upstream updates disabled                                     | 0.2.14 Windows installer built and uploaded successfully; signing, stable releases and a fork updater remain unfinished             |

See the detailed sections below for causes, rejected approaches, and remaining
risks. [successes.md](../successes.md) is deliberately limited to user-confirmed
outcomes. The [changelog](../CHANGELOG.md) separates fork releases from inherited
history.

### Deferred Proposals, Not Shipped Improvements

The October 16 discussion of Pocket native streaming, progressive Edge playback,
per-chunk flushing improvements for other wrappers, adaptive buffering, hardware
benchmarking, automatic engine tuning, and new models such as Supertonic is
research only. These changes are **not implemented in 0.2.14**. Existing warm-up,
audio reuse, and selected streaming paths should not be confused with a new
machine-adaptive Auto mode.

## Scope and Status

This document records the Windows problems reported by the user, additional
defects found during implementation and review, and the changes saved in this
repository. It also records incomplete fixes and unverified behavior.

The original report referred to CopySpeak 0.2.10. The available source baseline
was 0.2.8. Changes were implemented against that baseline, rebranded as
X-CopySpeak, and subsequently versioned as 0.2.11. We did not establish that the
baseline exactly matched the originally installed 0.2.10 build.

The initial remote-session edits were discarded. The fixes described here refer
to the later on-disk Codespaces implementation, not those discarded edits.

### Status Definitions

- **User-confirmed in tested scenarios:** the user tested the installed app and
  reported acceptable behavior. This does not imply coverage of every engine,
  configuration, or edge case.
- **Appears resolved in user tests:** the user reported apparent resolution;
  retain that qualification rather than treating it as exhaustive verification.
- **Implemented and checked:** the change is saved and relevant automated
  checks passed. This does not imply the user confirmed the installed behavior.
- **Partial mitigation:** a specific cause was addressed, but the broader report
  remains partly unresolved.
- **Needs Windows validation:** compilation or source inspection supports the
  change, but installed Windows behavior has not been confirmed.
- **Open:** no complete fix was implemented.

On 2026-10-09, the user reported successful pagination, abort/pause/resume, and
retrigger-setting save tests. The icon, lag/glitching, and slow-start problems
also appeared resolved in their tests. See [the confirmed-success log](../successes.md).
The tested app version, engines, profiles, and exact test sequences were not
specified in that confirmation; do not assume exhaustive Windows coverage.

## Incomplete Article Readings (2026-10-12)

The user reports intermittent stops before a copied article finishes. Source
inspection confirmed a failure path: one section's synthesis error exits the
reading, the synthesis guard releases the player, and the double-copy caller
previously logged the error only to the console. A later 0.2.13 Edge traceback
reported a fragment-4 synthesis failure after endpoint fallback, corroborating
this failure path. It does not establish the cause of every reported suspension.

The implementation retries selected transient network/service failures twice,
with 500 ms and 1 s backoff. Cancellation is polled during backoff. A streamed
section is not retried after PCM has been emitted, because replaying the request
would duplicate speech. Permanent failures and exhausted retries report a global
`reading-failed` event identifying the failed section. Already-buffered audio
drains normally, and the error is retained until the next reading starts. The
reading stops at the failed section: that section and every later one are not
spoken, and the message says so rather than implying the passage completed.
Provider retries may incur additional usage charges.

The clipboard cap is unchanged; its warning is now global so truncation cannot
be hidden merely because another page is open. The changes shipped in 0.2.14;
user testing of that installer remains needed to confirm the observed issue is
resolved in the installed application.

## Follow-up Engine Reports (2026-10-11)

Three separate reports, each with a distinct root cause. All are implemented and
covered by passing automated checks; **none has been validated on the installed
Windows build.**

### 8. Kokoro Takes Too Long to Start Speaking

**Report:** Kokoro took a long time to begin speaking when Play was pressed,
prompting a switch to Edge. Machine-specific details are intentionally omitted.

**Findings:** three software causes could stack. Hardware performance was not
benchmarked, so it is not ruled out as an additional contributor.

- **The bundled Kokoro profile shipped with an effect enabled.**
  `ProfileEffects::default()` is `enabled: true, active_effect: WalkieTalkie`
  ([tts.rs](../src-tauri/src/config/tts.rs)), and every bundled profile
  constructor used it — including `default_kokoro_profile()`. But
  `allows_streaming_playback` requires `!effects_enabled`
  ([synthesis.rs](../src-tauri/src/commands/tts/synthesis.rs)), because the
  streaming PCM scheduler wires straight to `ctx.destination` with no effect
  chain. So the default profile took the **fragment** path: the entire
  ~600-character fragment had to finish synthesizing before any audio played.
  Kokoro's own installer writes `effects: { "enabled": false }`, so the bundled
  profile and the installer disagreed.
- **The wrapper joined every ONNX window before yielding.** `stream()` in
  `copyspeak-kokoro.py` accumulated all inference windows and yielded
  `b"".join(pcm)` once, so even a streaming caller received nothing until the
  last window. Piper yields per sentence and did not have this problem.
- **Prewarming happened only at app launch.**
  `prewarm_active_profile` was called from the Tauri `setup` hook
  ([main.rs](../src-tauri/src/main.rs)) and nowhere else. `set_active_profile`
  and `set_config` never prewarmed, so selecting Kokoro (or changing voice or
  device) left the first reading paying the full cold model load.

**Changes:**

- `default_kokoro_profile()` now ships `effects: { enabled: false }`, matching
  `install-kokoro.ps1`.
- Kokoro's `stream()` yields one chunk per native inference window; its daemon
  loop and one-shot path both forward every window, and captions remain a
  cumulative snapshot per yield.
- `set_active_profile` and `set_config` now prewarm the active engine on a
  background thread after releasing the config lock.

**Status:** implemented and checked (308 Rust tests, 81 frontend tests, 10 wrapper
tests, and `bun run check` clean). Existing profile effects are preserved; turn
effects off if enabled to use PCM streaming. Wrappers are copied into the engine
directory at installation, so rerun the Kokoro installer without `-Force` to
refresh an existing wrapper. Actual time-to-first-audio still needs measurement
on Windows; protocol tests do not benchmark the user's CPU.

### 9. Piper Fails With "voice model not found ... Available: []"

**Report:**
`TTS command failed: exit exit code: 1: ERROR: voice model not found: C:\Program Files\CopySpeak\voices\en_US-amy-medium.onnx Available: []`

**Findings:** the error is a wrong-directory lookup, and the path in it is the
tell. `C:\Program Files\CopySpeak\voices` is the **app install directory**, not
the engine directory (`%LOCALAPPDATA%\CopySpeak\engines\piper\voices`).

- `install-piper.ps1` shipped Piper as `"engine": "local"` with a
  `preset: "piper"` and a `command: "uv"` template. Piper was later promoted to
  a first-class engine, but only **Pocket** ever had a profile-promotion
  migration ([tts.rs](../src-tauri/src/config/tts.rs)). A Piper profile
  therefore still ran through the generic CLI path: no daemon, no GPU toggle.
- The stored legacy template used a CWD-relative wrapper path
  (`scripts/copyspeak-piper.py`). Running it from the app install dir made
  `Path(__file__).resolve().parent.parent` resolve to `C:\Program Files\CopySpeak`,
  so `voices/` was looked up there and came back empty.

**Changes:**

- The v6 → v7 migration promotes installer-owned `Local` Piper templates to the
  first-class `Piper` engine. Matching covers the original relative and the
  engine-directory wrapper paths; custom commands and additional flags are
  preserved.
- The first-class template launches the installed wrapper from
  `{engine_dir}/piper/scripts/copyspeak-piper.py`, making its relative voice
  lookup resolve to the engine directory. The wrapper accepts `--voices-dir`
  for explicit manual use, but Rust does not add that flag: an older installed
  wrapper would reject it.
- `install-piper.ps1`/`.sh` now write `"engine": "piper"` for fresh installs.

**Status:** implemented and checked. The migration repairs existing profiles on
next launch. Needs Windows validation against the user's installed Piper.

### 10. Gemini TTS HTTP 400

**Report:** switching to Gemini produced
`Streaming synthesis failed mid-stream: ... Google API error 400 Bad Request:
"Model tried to generate text, but it should only be used for TTS. Make sure
your instructions are clear to only generate audio from a given text
transcript."` with model `gemini-2.5-flash-preview-tts`.

**Findings:** the request was well-formed for modality and voice —
`responseModalities: ["AUDIO"]` and `speechConfig.voiceConfig.prebuiltVoiceConfig`
were both correct in the streaming and unary bodies. What was missing was the
read-aloud framing: the clipboard text was sent as a bare `parts[0].text`, so a
snippet that reads like a question or command made the model start a text answer
and the TTS-only gateway rejected it. Google's own error text names the cause.
The pinned model was also the deprecated `gemini-2.5-flash-preview-tts` (shutdown
2026-11-17).

**Changes:**

- Both request builders send a `systemInstruction` anchoring the model to
  verbatim read-aloud with audio-only output.
- The text part is prefixed with an inline read-aloud direction **only** for the
  pre-3.8 preview models that are steered that way; Gemini 3.8 treats the text as
  a verbatim transcript and would otherwise speak the instruction.
- Bundled/default model moved to `gemini-3.8-flash-tts`, with a v7 migration for
  profiles still pinned to the legacy id.
- A unary reply that already starts with `RIFF` is passed through instead of
  being wrapped in a second WAV header (3.8 unary returns WAV; 2.5/3.1 returned
  headerless PCM).

**Status:** implemented and checked. **Not** exercised against the live Gemini
API from this environment. Regression tests verify prompt framing, audio parts
after metadata, and WAV pass-through, but do not prove the provider accepts the
request or that playback succeeds. Needs a live test.

### 11. Edge-TTS 403 on the Legacy Host

**Report:** after Kokoro felt slow, the user switched to Edge, which then failed
with `WSServerHandshakeError: 403` and `Sec-MS-GEC-Version=1-143.0.3650.75`. They
later received third-party advice to upgrade `edge-tts` / clear the `uv` cache,
and pushed back on the conclusion that Edge could not be recovered — pointing at
a working HuggingFace Space that uses the same library.

**Findings (corrected):** the upgrade/cache advice still cannot work, and that
conclusion was right, but "the engine cannot be fixed locally" was **wrong**.

- CopySpeak runs Edge through an **embedded** wrapper:
  `uv run --no-project --with edge-tts==7.2.8 python -c <include_str! wrapper>`
  ([edge.rs](../src-tauri/src/tts/edge.rs)). Nothing is installed into a project
  environment and no `edge-tts` is on `PATH`, so an upgrade or cache refresh has
  no target.
- **7.2.8 is already the newest release**, so there is no newer fix to fetch.
- The failure is **regional**: several EU users are at 100% failure while North
  American users are unaffected, and the HuggingFace Space that still works is
  `region: us`. That is why the reports disagree.
- The decisive fact: Microsoft's **newer host still accepts the old client**.
  Release 7.2.2 already shipped
  `wss://api.msedgeservices.com/tts/cognitiveservices/websocket/v1` with
  `Ocp-Apim-Subscription-Key` and the _same_ token value, and report after report
  in [rany2/edge-tts#490](https://github.com/rany2/edge-tts/issues/490) says
  retargeting to it restores synthesis **without** needing Microsoft's new
  `2-155.0.4283.45` token — the `Sec-MS-GEC` value is unchanged, only the host is.
- The patch point is precise, verified in the 7.2.8 source: `communicate.py` does
  `from .constants import ... WSS_URL` and builds the URL inside `__stream`, so
  `edge_tts.communicate.WSS_URL` is read at call time and is the only assignment
  that takes effect (`edge_tts.constants.WSS_URL` would do nothing, and
  `edge_tts.WSS_URL` does not exist).

**Changes:**

- The embedded wrapper retargets the host as a **fallback**: it tries the legacy
  host the pinned client ships with, and retries
  `api.msedgeservices.com/tts/cognitiveservices/websocket/v1` with
  `Ocp-Apim-Subscription-Key` **only** when the handshake is refused. Regions
  Microsoft has not migrated keep today's exact request; the fallback appends no
  URL when unused.
- Headers and the token generator stay unchanged. Only HTTP status 403 triggers
  the alternate-host retry; other handshake or server failures are propagated.
- An explicit wrapper marker reports **both** hosts returning 403, so an unrelated
  failure is not mislabeled as a regional double refusal.

**Status:** implemented; endpoint selection and error handling are covered by
the [endpoint tests](../scripts/test-edge-endpoints.py) against a stub. A live
Codespaces probe on 2026-10-11 using 7.2.8, unchanged headers, and
`en-US-AvaMultilingualNeural` returned audio and six word boundaries from both
hosts: legacy 16,560 bytes / first audio 996 ms; alternate 16,704 bytes / 478 ms.
This proves both hosts accepted the client here, not recovery from the user's
regional 403. On 2026-10-11, the user retested their existing installation and
confirmed Edge works again, suggesting a temporary or intermittent service
failure. This does not validate the new fallback on their machine; retain it
only for a future 403, leaving successful normal requests unchanged. See the
[user-confirmed log](../successes.md). This is a **stopgap**: if Microsoft starts requiring the v2
token on the fallback host too, it stops working, and official Azure Speech (the
same neural voices) remains the durable path.

## User-Reported Issues

### 1. Significant Lag and Audio Glitching

**Report:** playback lagged and glitched significantly.

**Findings:**

- The global-shortcut plugin was registered twice. This was a duplicate-handler
  risk; actual double-firing was not reproduced on Windows.
- Browser readings reported playback progress through IPC on every 25 ms
  caption tick, potentially producing 40 round trips per second.
- Audio decoding, stretching, effects, and encoding still involve frontend
  work. Provider delays, buffer underruns, and local inference load can also
  contribute to gaps.

**Changes:** removed the duplicate shortcut registration and throttled browser
progress reports based on clock movement, pause changes, and a periodic refresh.
The local caption clock was not replaced by the throttled browser reporting.

**Status:** appears resolved in user tests (2026-10-09). The implementation is
still a targeted mitigation, not evidence that all possible glitching causes
have been eliminated.

**Code:** [main.rs](../src-tauri/src/main.rs),
[playback-store.svelte.ts](../src/lib/stores/playback-store.svelte.ts).

### 2. Slow Time to First Audio, Even With Pagination

**Report:** speech took too long to start despite pagination.

**Findings:** the non-streaming paginated `speak_now` path synthesized and joined
the entire reading before emitting audio. Its pagination helper also discarded
the user's configuration and constructed `PaginationConfig::default()`.

**Changes:** pass the configured pagination settings into the helper and emit
each batch fragment as soon as it is synthesized. The assembled reading is
still retained for history and Replay, but is not emitted a second time.
Synthesis of later fragments can overlap playback of earlier fragments; this
does not mean multiple provider requests are synthesized in parallel.

**Status:** appears resolved in user tests (2026-10-09). The change is implemented
and checked for paginated batch `speak_now` readings.
Single-fragment batch synthesis still waits for the fragment to finish. Engine
cold starts and effects disabling PCM streaming remain latency factors.

**Code:** [synthesis.rs](../src-tauri/src/commands/tts/synthesis.rs).

### 3. Pagination Split Words in the Middle

**Report:** mid-word cuts were unacceptable; word or sentence boundaries were
preferred.

**Findings:** `force_split_ranges` originally cut at exact character offsets.
Oversized unpunctuated tails could also remain unsplit. Periods inside decimal
numbers, versions, domains, and identifiers could be mistaken for sentence ends.

**Changes:**

- Preserve sentence grouping where possible and snap fallback cuts to whitespace.
- Split oversized trailing text instead of accepting one unlimited tail fragment.
- Preserve oversized Latin tokens up to the existing 4096-character hard cap.
- Use CJK clause punctuation where no inter-word whitespace exists.
- Protect punctuation-bearing tokens, including URLs, query strings,
  comma-separated numbers, slash-separated words, and relative paths.
- Preserve internal dots in versions, domains, dotted identifiers, and paths;
  ordinary trailing sentence periods remain boundaries.
- Remove the single-piece fallback that could replace the computed slice with
  the original text and lose its precise source span.

**Status:** user-confirmed in tested scenarios (2026-10-09): pagination
splits/breakpoints were reported to be okay. An unbreakable
token beyond 4096 characters is deliberately hard-split. CJK fallback is
punctuation-aware, not a linguistic word-segmentation engine.

**Code:** [pagination.rs](../src-tauri/src/pagination.rs).

### 4. Pinned Windows Taskbar Icon Disappeared

**Report:** the icon disappeared when pinned to the taskbar.

**Findings:** the app did not explicitly set a process AppUserModelID. This was
a plausible identity/grouping cause, not a reproduced explanation for every
blank-icon scenario. The ICO was inspected and contains 16, 24, 32, 48, 64,
and 256 pixel images.

**Changes:** set `com.copyspeak.tts` before window creation, matching the retained
Tauri identifier. Enable the Windows Shell API feature and call the function
inside `unsafe` with a live UTF-16 buffer passed as `PCWSTR`.

**Status:** appears resolved in user tests (2026-10-09). The change cannot repair
an existing pinned shortcut with a stale target or icon location. The confirmation
does not specify whether an installer-update or close-to-tray sequence was tested.

**Code:** [main.rs](../src-tauri/src/main.rs),
[Cargo.toml](../src-tauri/Cargo.toml),
[tauri.conf.json](../src-tauri/tauri.conf.json).

### 5. Pause and Abort Were Missing or Worked Poorly

**Report:** pause and abort controls appeared missing or unreliable.

**Findings:** the Play page had no Pause/Resume button. Stop originally stopped
playback without cancelling the remaining synthesis. Several lifecycle defects
were discovered while improving these paths, listed below.

**Changes:** add Pause/Resume and route its action through the backend event
once. Make `stop_speaking` delegate to the shared abort path, which signals
cancellation, kills an active tracked CLI process, stops the backend queue, and
stops frontend playback. Reset the queue's stop flag for each new queued reading.
Check cancellation after post-processing and before starting subsequent work.

**Status:** user-confirmed in tested scenarios (2026-10-09): abort, pause, and
resume were reported to work okay. Tray pause detection,
post-processing request cancellation, and cross-mode playback limitations remain
open as described in the remaining-work section.

**Code:** [playback-controls.svelte](../src/lib/components/playback-controls.svelte),
[play-page.svelte](../src/lib/components/play-page.svelte),
[playback.rs](../src-tauri/src/commands/playback.rs),
[synthesis.rs](../src-tauri/src/commands/tts/synthesis.rs),
[main.rs](../src-tauri/src/main.rs).

### 6. Selecting "Stop and Restart" Prevented Saving Settings

**Report:** selecting this retrigger option produced `unknown variant 'stop',
expected 'interrupt' or 'queue'` when calling `set_config`. Other settings could
not be saved with that selection either.

**Cause:** the dropdown supplied `stop`, `queue`, and `ignore`, but the frontend
type and Rust enum supported only `interrupt` and `queue`. An `as any` assertion
hid the mismatch. Deserialization rejected the entire configuration before the
save command could process it.

**Changes:** map "Stop and restart" to `interrupt`; derive choices from typed
options; validate selected values with `isRetriggerMode`; remove the unsupported
Ignore choice and its translation key. Add tests for accepted and rejected values.

**Status:** implemented and checked, included in the 0.2.11 Windows installer.
Saving "On Retrigger Behavior" is user-confirmed in tested scenarios (2026-10-09).
Reopen/restart persistence was not explicitly reported. This fixes the
save contract; it does not independently establish that all webview playback
paths honor the configured retrigger behavior.

**Code:** [types.ts](../src/lib/types.ts),
[types.test.ts](../src/lib/types.test.ts),
[settings-page.svelte](../src/lib/components/settings-page.svelte),
[playback-settings.svelte](../src/lib/components/settings/playback-settings.svelte),
[backend playback config](../src-tauri/src/config/playback.rs).

## Additional Defects Found During Review

Some entries below were regressions introduced in early implementation passes
and corrected before the saved test build. Others existed in the baseline.

| Issue                                                                | Cause and implemented correction                                                                                                                                                                       | Status                                                                         |
| -------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------ |
| Pause toggled twice                                                  | The page toggled locally and then invoked a command that emitted another toggle. Keep the backend event as the single owner.                                                                           | Implemented and checked                                                        |
| Stop silenced later queued readings                                  | `FragmentQueue::clear()` did not reset `stop_flag`. Call `clear_stop_flag()` on each new queued reading.                                                                                               | Implemented and checked                                                        |
| Temporary queue starvation looked like completion                    | The frontend queue finished whenever it emptied, even while another fragment was still synthesizing. Track the final marker and wait for a terminal signal.                                            | Implemented and checked                                                        |
| Final-marker state leaked between readings                           | Reset the marker when fragment index 0 is enqueued, rather than clearing live queue state on a separate synthesis-start event.                                                                         | Implemented and checked                                                        |
| Completion was sent to both players                                  | The empty fragment queue completed during PCM playback and cleared the live scheduler reference. Route terminal events only to the active player.                                                      | Implemented and checked                                                        |
| Mid-stream synthesis failures left playback stuck                    | Failure could bypass the final fragment and pagination events. Treat synthesis end as a completion safety net for the active player.                                                                   | Implemented and checked                                                        |
| A failed short PCM stream never started or completed                 | Pending audio below the prebuffer threshold had no scheduled sources to finish. Authoritative completion starts pending audio and flushes the stretcher before waiting for drain.                      | Implemented and checked                                                        |
| Stretched audio tails were lost on failure                           | Failure bypassed fragment-end flushing. Flush buffered stretcher output during authoritative completion.                                                                                               | Implemented and checked                                                        |
| A subsequent reading inherited a completed PCM lifecycle             | Reusing a scheduler while old audio drained retained `_finished`. Reopen completion state with `beginReading()` without removing live sources.                                                         | Implemented and checked for tested sequences                                   |
| Play-page Replay held only the last fragment                         | Fragment-by-fragment playback replaced the local buffer. Request the backend whole-reading cache instead.                                                                                              | Implemented and checked                                                        |
| Queued readings did not populate Replay's cache                      | Assemble and cache all completed queued fragments, including saved-history replays, not only newly synthesized fragments.                                                                              | Implemented and checked                                                        |
| Cache assembly failure retained a different reading                  | Clear bytes, text, and captions atomically if concatenation fails. Replay reports failure rather than falling back to partial local audio.                                                             | Implemented and checked; mixed-rate audio is not normalized                    |
| Replay delivery errors were swallowed                                | Propagate `app.emit` failure instead of returning success without delivering audio.                                                                                                                    | Implemented; requires runtime failure-path validation                          |
| Stop during queued post-processing could be overwritten              | Queued synthesis lacked the shared synthesis mutex. Serialize it with `speak_now` and check abort after the await.                                                                                     | Implemented; active HTTP work is not immediately interrupted                   |
| Lock-wait requests could start after Stop                            | Snapshot a cancellation generation before waiting and reject requests if Stop changes it. Apply checks around abort reset.                                                                             | Implemented; history re-synthesis still has a separate path                    |
| HUD appeared after completion or Stop                                | Background threads delayed startup events; final assembly also restarted the HUD after early playback. Emit synchronously and initialize batch playback at the first fragment, not cache finalization. | Implemented and checked; history-playback delay remains                        |
| Queued streamed HUD reinitialized after PCM started                  | Gate the first-fragment `show_hud` call to non-streamed playback.                                                                                                                                      | Implemented and checked                                                        |
| Clipboard truncation cut words or discarded a complete boundary word | Snap to whitespace and keep a word ending exactly at the cap.                                                                                                                                          | Implemented and checked; an oversized single token can still be hard-truncated |
| Leading whitespace could produce no speakable truncated text         | Extend through the next word when the capped prefix is whitespace-only.                                                                                                                                | Implemented; this can exceed the nominal truncation limit                      |
| Windows-only AUMID call did not compile in an early pass             | The API requires `unsafe`; bind the string for pointer lifetime and pass `PCWSTR`.                                                                                                                     | Corrected; later native Windows build succeeded                                |

### Investigated Changes That Were Rejected or Reverted

- A blanket frontend guard rejecting batch fragments after Stop also rejected
  legitimate saved-audio and history replay. It was reverted rather than shipped.
- A test expecting zero sentence boundaries for a nonempty dotted token ignored
  the detector's synthetic final boundary. The test was corrected to assert that
  pagination preserves the complete token instead.
- A suspected duplicate stretched tail from final-marker plus queue-completion
  flushing was tested. The stretcher resets its counters; the regression test
  confirms that no second tail is scheduled.
- A separate backend pause event was considered, then discarded in favor of the
  existing single `playback-toggle-pause` path.

**Related code:** [fragment queue](../src/lib/stores/playback/fragment-queue.ts),
[PCM scheduler](../src/lib/stores/playback/pcm-stream.ts),
[scheduler tests](../src/lib/stores/playback/pcm-stream.test.ts),
[playback-store tests](../src/lib/stores/playback-store.test.ts),
[clipboard handling](../src-tauri/src/clipboard/mod.rs),
[HUD management](../src-tauri/src/hud.rs).

## Build and Development Obstacles

| Obstacle                                                             | Action taken                                                                                                            |
| -------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| Remote VFS session could not compile locally                         | Restarted from the materialized Codespaces checkout; did not rely on discarded remote edits.                            |
| Bun and Rust were initially unavailable on PATH                      | Installed/configured the toolchains and checked their versions.                                                         |
| Test startup failed resolving `node:module`                          | Generated `.svelte-kit/tsconfig.json` with `svelte-kit sync` before tests. The project check script includes this step. |
| Rust compilation lacked GTK/WebKit dependencies                      | Installed the Linux build dependencies used by the repository's CI.                                                     |
| Tauri's context macro could not find `dist`                          | Built the frontend before compiling the packaged configuration.                                                         |
| Full lint/type checking lacked Chrome types                          | Installed the browser extension's declared dependencies.                                                                |
| Windows cross-check failed in `ring` without MSVC tools              | Built on the native Windows GitHub Actions runner instead of claiming Linux checks validated Windows-only code.         |
| Terminal tools were temporarily disabled                             | Reported the actual tool restriction; reran executable checks when access returned. Codespaces itself has a terminal.   |
| Test installer version 0.2.8 was below the reported installed 0.2.10 | Bumped all eight version stamps to 0.2.11 and rebuilt.                                                                  |
| Expiring unsigned artifacts were unsuitable for public distribution  | Kept them as test artifacts. Code signing and Microsoft Store preparation were discussed but not implemented.           |

## Rebrand and Delivered Builds

The user requested X-CopySpeak branding and chose to retain the existing
installation identity and data rather than create a separate side-by-side app.
Visible product, window, tray, and relevant UI labels were updated.

The executable basename, `com.copyspeak.tts`, data paths, engine scripts, and
browser protocol remain unchanged. This is a compatibility decision, not a
complete renaming of every internal symbol or integration.

The [Windows test workflow](../.github/workflows/build-windows-test.yml) uploads
the app executable and NSIS setup installer without publishing a release. The
[test configuration](../src-tauri/tauri.windows-test.conf.json) disables updater
artifact signing and upstream update endpoints for these test builds.

| Commit    | Contents                                                                               | Windows build                                                                                |
| --------- | -------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `1944d59` | Playback/pagination changes, X-CopySpeak branding, and test-build workflow             | [0.2.8 run](https://github.com/iammultiman/X-CopySpeak/actions/runs/37649125883), succeeded  |
| `17a6adb` | Retrigger save fix, regression tests, and synchronized 0.2.11 version bump             | [0.2.11 run](https://github.com/iammultiman/X-CopySpeak/actions/runs/37655750480), succeeded |
| `fa165f7` | Retrigger cancellation across triggers, stale-output protection, version 0.2.12        | [0.2.12 run](https://github.com/iammultiman/X-CopySpeak/actions/runs/38053955582), succeeded |
| `4fe661e` | Local startup/cancellation, Piper/Gemini corrections and Edge fallback, version 0.2.13 | [0.2.13 run](https://github.com/iammultiman/X-CopySpeak/actions/runs/38151690588), succeeded |
| `7cbd737` | Incomplete-reading reporting/retries and browser audio ownership, version 0.2.14       | [0.2.14 run](https://github.com/iammultiman/X-CopySpeak/actions/runs/38732715502), succeeded |

The latest delivered artifact is `X-CopySpeak-0.2.14-Windows-x64-5`. It contains
`copyspeak.exe` and `bundle/nsis/X-CopySpeak_0.2.14_x64-setup.exe`. Actions
artifacts are temporary, require GitHub sign-in, and are not a permanent public
release channel. Downloaded copies under `artifacts/` are local build outputs,
not source files to commit.

The 0.2.11 installer is named `X-CopySpeak_0.2.11_x64-setup.exe`. Its downloaded
SHA-256 was:

```text
535481eb6d2f37546967c6f68b5af4ea048b70a26db183fc292779782c116cf9
```

An intended in-place upgrade does not establish that upgrading the user's
specific original installation has been tested. Quit the existing tray process
and back up settings before installer testing. The builds are unsigned and may
trigger Windows security warnings. A 32-bit NSIS launcher containing a 64-bit
app is normal; the application executable was verified as Windows x86-64 PE.

## Verification Record

These are results obtained during the earlier implementation/build sessions,
not checks rerun merely to create this document:

- For commit `7cbd737` (0.2.14): 93 frontend tests and 320 Rust tests passed,
  with one ignored live-provider test. Frontend check and production build,
  Rust all-target compilation, whitespace checks, and the native Windows
  installer workflow passed. Existing Rust warnings remain. These are automated
  results, not user acceptance of the installed 0.2.14 application.
- Full frontend formatting, lint, TypeScript, and Svelte checks passed.
- The 0.2.11 frontend suite passed 81 tests across 12 files.
- The playback implementation's full Rust suite passed 286 tests, with one
  live-provider probe intentionally ignored. It was not rerun as a full suite
  solely for the later version/UI-options change.
- `cargo check --all-targets` passed for 0.2.11 with existing warnings.
- Native Windows Actions builds passed frontend checks/tests, compilation,
  installer packaging, and artifact upload.
- Regression cases cover token preservation, selected clipboard boundaries,
  fragment ordering, PCM buffering/failure tails, pause state, successive PCM
  readings, Replay IPC success/failure, and supported retrigger option values.

Three audit rounds were used for the larger playback fix pass. GPT and Grok
returned source findings; Gemini's endpoint failed DNS resolution. Final
post-audit corrections were tested but did not receive another independent
audit after the cap was reached. Passing tests are not a clean-audit guarantee
or a substitute for installed Windows testing.

### User Testing - 2026-10-09

The user confirmed acceptable pagination splits/breakpoints, working abort,
pause and resume, and successful saving of "On Retrigger Behavior". They also
reported that the icon, lag/glitching, and slow time to first audio appear
resolved in their tests. No exact engine/profile matrix or tested version was
provided. Additional audit findings and unmentioned test scenarios must not be
treated as user-confirmed merely because the original symptoms improved.

## Remaining Work and Limitations

1. **General lag and latency:** the reported symptoms appear resolved in user
   tests. Main-thread work, engine cold starts, and provider underruns were not
   fully redesigned or benchmarked across all configurations.
2. **Taskbar icon:** the reported symptom appears resolved in user tests.
   Installer-update and close-to-tray sequences were not explicitly confirmed;
   existing stale pins may need recreation.
3. **Tray pause:** the tray's playing-state decision still relies on Rodio and
   can miss webview playback.
4. **Cross-mode ownership:** global lifecycle events do not carry per-reading
   IDs. Switching between PCM and fragment playback or overlapping history
   re-synthesis remains an architectural risk.
5. **History re-synthesis cancellation:** joined the shared generation/lock
   protocol in 0.2.12; the earlier separate abort-reset defect is no longer open.
   Installed history/retrigger combinations still need broader coverage.
6. **Post-processing Stop:** subsequent synthesis is prevented, but an in-flight
   provider HTTP call is not immediately cancelled and can hold the synthesis
   mutex until it returns.
7. **Single-shot paused PCM completion:** the older idle fallback can discard its
   timeout while paused. The new synthesis-end signal covers the normal backend
   path; idle-only completion paths have not all been independently validated.
8. **Cache format mismatches:** failed concatenation invalidates Replay instead
   of normalizing mixed sample rates. Aborted readings can leave the previous
   successful cache; there is no per-reading cache identity check.
9. **History HUD startup:** `show_hud_playback` retains a separate delayed event;
   the synchronous change covered synthesis/playback startup helpers only.
10. **Retrigger behavior versus saving:** saving the setting was confirmed by
    the user. An end-to-end test that interrupt/queue behavior is consistently
    honored by every audible
    playback path has not been completed. Ignore mode is not implemented.
11. **Oversized input policy:** tokens beyond 4096 are hard-split; clipboard
    truncation has its own documented hard-cut and leading-whitespace exceptions.
12. **Distribution readiness:** trusted installer signing, stable public hosting,
    Store-compliant packaging, privacy disclosures, and certification have not
    been implemented. No Microsoft Store submission has been made.

## Additional Windows Coverage

### Local Daemon Cancellation (Included in 0.2.13)

The local implementation now releases a cancelled reading's wait for background
model loading, cancels response-header waits by terminating the owned daemon,
and polls cancellation during batch collection. Cancelled daemon requests do not
launch a one-shot fallback. Background prewarming is shared and may continue so
the next reading can reuse it; abandoning a reading does not unload that shared
model. Startup handshakes have a five-minute deadline.

This work is not included in the 0.2.12 installer. Automated tests exercise
cancellation/deadline waits, a stalled response header and process cleanup on
Linux, and cancellation of silent or partial batch streams. Windows process-tree
cleanup and actual engine cold starts still need Windows verification. Cloud
HTTP cancellation is separate from this local-daemon change.

The original symptom tests have been reported as passing or apparently resolved.
The checklist below remains useful for broader coverage; do not interpret it as
withdrawal of the user's confirmation.

1. Quit the running app completely, back up settings, and install X-CopySpeak 0.2.14.
2. Select "Stop and restart", save, close Settings, and reopen it. Also change
   an unrelated setting and confirm it saves successfully.
3. Read long unpunctuated text, URLs, numbers, and CJK text; check fragment seams
   and time to first audio using the relevant engine/profile settings.
4. Pause/resume, Stop during synthesis, and start another reading afterward.
5. Replay fresh and saved queued readings after reading a different passage.
6. Test pinning, minimize-to-tray, relaunch, and an installer upgrade.
7. Report the app version, engine, streaming/effects settings, exact reproduction
   steps, and logs for any remaining failure.

## Retry Classifier and Browser Session End (2026-10-13)

**Implemented and checked.** Two pre-existing defects found while reviewing the
incomplete-readings fix. Neither was validated on the installed Windows build.

### The retry classifier never fired for the reported Edge failure

The user's traceback showed the 403 host fallback running, both hosts failing, and
the terminal error being `NoAudioReceived("No audio was received...")`. The
classifier returned on the **first** status token it found, so the `403` from the
refused handshake was read as a permanent failure and the function returned before
reaching the phrase list. The `"no audio was received"` phrase existed for exactly
this case and was dead whenever a permanent code appeared earlier in the string.

Classification now evaluates every signal: any standalone retryable status
(`429`, `500`, `502`, `503`, `504`) is retryable regardless of position, and any
transient phrase (timeout, connection reset/closed/aborted/refused, no audio
received, daemon closed its output) is retryable even when a permanent status
appears earlier. A permanent status on its own is still not retried.

This does not guarantee Edge availability: the reported run failed after trying
both hosts. The change makes transient failures eligible for bounded retries;
it does not establish recovery for every endpoint refusal or rule out future
client-side fixes when Microsoft's service behavior changes.

### A mid-article failure tore the browser companion bar off the page

`browser_bridge.rs` ended the session with `SessionStatus::Error` the moment
`speak_queued` returned `Err`, so the on-page bar and word highlights disappeared
while the buffered fragments were still audibly playing. It also raced the
playback store, which was written to hold the session open until the audio drains;
`finish_session` is first-wins, so the eager call won.

The bridge now ends the session immediately only when nothing was ever audible
(`active_fragment == NO_FRAGMENT`, written solely by the frontend's audible-clock
progress). Once audio has started it leaves the session up and the store reports
the terminal `error` state from `finishPlayback` when the queue drains. The store
now also reports `cancelled` when a reading is stopped, and the bridge reaps a
deferred session after a 30 s frontend liveness lease, so a closed or crashed
renderer cannot leave an orphan session registered to answer a later reading's
progress and finish calls.

The on-page companion bar does not display error text: because `error` is
terminal in the extension, the bar and word highlights are dismissed once the
audio drains. The failure itself is signalled by the extension action badge (`!`)
and the desktop app's toast.

Covered by `transient_signal_beats_a_coincidental_permanent_status`,
`a_failed_reading_ends_immediately_only_before_any_audio_plays` and
`a_deferred_reading_is_reaped_when_the_frontend_goes_quiet`. The browser path
still needs Windows and real-Microsoft-endpoint verification.

### The retry classifier missed provider status errors and transport failures

Two further gaps in the same classifier, both pre-existing.

`ElevenLabs API error 503: ...` contains neither `http` nor `status`, so the
gate rejected a retryable `503` before the status scan ran. The gate now also
accepts a provider status error phrasing (`api error`).

More broadly, providers stringify a reqwest failure directly, and reqwest's
`Display` prints only the error _kind_: a timeout or a dropped connection becomes
a bare `error sending request`, with the cause hidden in `source()`. The retry
phrases (`operation timed out`, `connection refused`, ...) therefore never saw
it. Every provider request site now flattens the `source()` chain into the
message, and the bare kinds are recognised as transient in their own right.

### A delivered-but-not-yet-audible fragment could still lose the companion bar

The session-end deferral keyed on audibility alone (`active_fragment !=
NO_FRAGMENT`), but that is written only by the frontend's throttled audible-clock
report. A fragment can reach the frontend, fail, and be reopened before any
report arrives, so the bar could still be torn down under audio about to speak.

The bridge already listens to `audio-fragment-ready` and `audio-stream-chunk`, and
Tauri invokes Rust listeners synchronously on the emitting thread, so the reading
is now marked `delivered` inside `emit` — strictly before the failure is
classified. A frontend acknowledgment was tried first and rejected: it can arrive
after the irreversible decision, because the producer can emit and fail before
the webview processes the event. The bridge defers whenever audio was delivered
_or_ is audible. The liveness lease still reaps the session if the renderer
disappears.

Both the request-opening and the response-body read sites flatten the chain: a
body timeout surfaces as `error decoding response body`, so the read sites matter
as much as the request sites.

### Browser reading identity and lifecycle (2026-10-14)

Two lifecycle defects, both pre-existing, found while fixing the deferred session
end. Both are implemented and covered by automated checks; neither is validated on
the installed Windows build.

#### Reports were resolved by "whichever session exists"

Start, progress and finish all looked up the session with
`sessions.values().next()`, and `start_browser_reading` inserted a new session
without retiring an existing one. One connected client does not imply one
session, so a reconnect could leave an old session registered; HashMap iteration
then picked an arbitrary entry, and a delayed report from a replaced reading could
move the replacement's clock or end it.

The reading id is now carried through the `browser-reading` activation event and
through `browser_reading_progress` / `browser_reading_finished`, and each is
resolved by that id. `retire_existing_sessions` ends any other session when a new
reading starts, so exactly one is live.

#### An Interrupt-mode retrigger could end the new reading before it spoke

`on_retrigger = Interrupt` aborts the previous playback as part of starting the
new reading. That emits `synthesis-state-change: false`, which completes the
(fragmentless) queue and runs `finishPlayback`, while the new browser reading is
active but has produced no audio yet. The frontend then reported the new session
ended, and because `finish_session` is first-wins the real reading could never
claim it.

The store now tracks whether the tracked reading has actually produced audio and
does not report a terminal state before it has, so a stop or completion belonging
to the superseded reading cannot close its replacement. A recorded synthesis
failure still reports, so a section error is never swallowed.

Covered by `does not end a browser reading stopped before it produced any audio`
and `ignores a terminal event for a superseded browser reading`.

### Silent browser readings: teardown vs. user stop (2026-10-14)

Follow-on from the reading-identity work. Both defects were pre-existing and are
covered by automated checks; neither is validated on the installed Windows build.

#### A replaced reading's teardown still tore down its replacement

The store's terminal-report gate could not stop the _other_ lifecycle events. A
retrigger in Interrupt mode aborts the previous playback, which emits
`synthesis-state-change: false`, `playback-stop` and `synthesis-aborted` while the
new browser reading is active but still silent. The first of those completes the
fragmentless queue and runs `finishPlayback`, and the last calls `handleStop`, so
the new reading was stopped before its first fragment even though its own report
was suppressed.

The store now judges whether a reading-lifecycle event belongs to the reading it
is running (`isCurrentReadingEvent`): a browser reading that has produced no audio
yet is still inside the replaced reading's teardown window, and a recorded
failure is always this reading's own. `synthesis-state-change: false` and
`synthesis-aborted` are skipped for a silent browser reading.

#### A user Stop before the first fragment left the session registered

Suppressing those reports also suppressed an explicit user Stop of a silent
browser reading, which left the session in the map until another reading retired
it. The frontend cannot tell a user Stop from retrigger teardown — both look like
an abort while the reading is silent — so the distinction lives in the backend:
`stop_speaking`, `abort_synthesis` and the tray abort call
`end_active_browser_reading`. `do_abort_synthesis` is shared with the retrigger
path, which must not close the reading that replaces it, so it deliberately does
not.

`browser_reading_finished` also ignores a `completed`/`cancelled` report for a
reading that never delivered audio (`should_finish_on_report`), with an error
always reported so a fragment-0 failure still surfaces.

Covered by `a_silent_reading_is_not_ended_by_a_superseded_teardown`,
`reports a user stop for a silent browser reading so the bridge can end it` and
`ignores the previous reading's teardown while a new browser reading is silent`.

### Desktop audio while a browser reading waits (2026-10-14)

In Queue After Current mode, a browser session can be registered before an
earlier desktop reading finishes. Global active-session flags and text matching
were not ownership checks: desktop audio could claim browser delivery, report
its position against the waiting session, and complete that session prematurely.

Browser synthesis now supplies an immutable `browser_reading_id` on every batch
fragment and PCM event. Desktop synthesis, replay and history audio have no
browser owner. Rust resolves delivery and captions by that exact ID; the frontend
preserves it through the fragment queue and scheduled PCM positions. Browser
progress follows the audible source's owner, not the newest received chunk, and
desktop completion leaves the waiting browser session intact. PCM captions also
use both reading ID and fragment index so separate fragment-zero timelines do
not overwrite each other.

Regression coverage includes desktop completion followed by browser playback,
identical-text foreign delivery, and browser PCM buffered behind desktop PCM.
Installed Windows behavior still needs user verification.
