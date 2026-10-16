import { afterEach, beforeEach, expect, it, vi } from "vite-plus/test";
import { playbackStore } from "./playback-store.svelte";
import { emitTo } from "@tauri-apps/api/event";
import { PcmStreamScheduler } from "./playback/pcm-stream";
import { toast } from "svelte-sonner";
import * as tauriCore from "@tauri-apps/api/core";

vi.mock("svelte-sonner", () => ({ toast: { error: vi.fn(), warning: vi.fn() } }));
vi.mock("@tauri-apps/api/core", { spy: true });

const { listeners } = vi.hoisted(() => ({
  listeners: new Map<string, (event: { payload: unknown }) => Promise<void>>()
}));
vi.mock("$lib/services/tauri.js", () => ({ isTauri: true }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (name, callback) => {
    listeners.set(name, callback);
    return () => listeners.delete(name);
  }),
  emit: vi.fn(async () => {}),
  emitTo: vi.fn(async () => {})
}));
vi.mock("./playback/analyser.js", () => ({
  AudioAnalyser: class {
    getAnalyser() {
      return null;
    }
    setup() {}
    start() {}
    stop() {}
    destroy() {}
  }
}));

const decode = vi.fn(async () => ({ duration: 1 }));
let audio: HTMLAudioElement;
let playSpy: ReturnType<typeof vi.spyOn>;

beforeEach(async () => {
  decode.mockReset().mockResolvedValue({ duration: 1 });
  vi.stubGlobal(
    "AudioContext",
    class {
      state = "running";
      decodeAudioData = decode;
      close = vi.fn();
    }
  );
  audio = document.createElement("audio");
  vi.spyOn(audio, "pause").mockImplementation(() => {});
  playSpy = vi.spyOn(audio, "play").mockImplementation(async () => {
    audio.dispatchEvent(new Event("play"));
  });
  vi.spyOn(playbackStore, "buildPlaybackUrl").mockResolvedValue("blob:history-audio");
  playbackStore.setAudioElement(audio);
  playbackStore.syncPlaybackConfig(100, 1, 1);
  playbackStore.historyReadingId = "batch:reading";
  await playbackStore.setupListeners();
});

afterEach(() => {
  playbackStore.teardownListeners();
  if (vi.isFakeTimers()) vi.runOnlyPendingTimers();
  playbackStore.setAudioElement(null);
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

async function sendAudio(index?: number, browserReadingId?: string) {
  const name = index === undefined ? "audio-ready" : "audio-fragment-ready";
  await listeners.get(name)!({
    payload:
      index === undefined
        ? "YQ=="
        : {
            browser_reading_id: browserReadingId,
            audio_base64: "YQ==",
            fragment_index: index,
            fragment_total: 2,
            is_final: index === 1,
            text: `Part ${index}`
          }
  });
}

it("reports an incomplete reading and preserves its error while buffered sections drain", async () => {
  await listeners.get("synthesis-state-change")!({ payload: true });
  await sendAudio(0);
  await sendAudio(1);
  const message =
    "Reading stopped: section 3 of 5 failed, so the rest of the passage was not read. HTTP 503";
  await listeners.get("reading-failed")!({ payload: message });
  expect(playbackStore.error).toBe(message);
  expect(toast.error).toHaveBeenCalledWith(message, { duration: 15000 });
  audio.onended?.(new Event("ended"));
  await vi.waitFor(() => expect(playSpy).toHaveBeenCalledTimes(2));
  expect(playbackStore.error).toBe(message);
  await listeners.get("synthesis-state-change")!({ payload: true });
  expect(playbackStore.error).toBeNull();
});

it("warns about clipboard truncation independently of the active route", async () => {
  await listeners.get("text-truncated")!({
    payload: { original_length: 120000, truncated_length: 99999, max_length: 100000 }
  });
  expect(toast.warning).toHaveBeenCalledWith(expect.stringContaining("120000 to 99999"), {
    duration: 15000
  });
});

it("does not label a failed browser reading completed when its audio drains", async () => {
  await listeners.get("synthesis-state-change")!({ payload: true });
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "b1" } });
  await sendAudio(0, "b1");
  await listeners.get("reading-failed")!({
    payload: "Reading stopped: section 2 of 5 failed, so the rest of the passage was not read."
  });
  await listeners.get("synthesis-state-change")!({ payload: false });
  audio.onended?.(new Event("ended"));
  expect(tauriCore.invoke).toHaveBeenCalledWith("browser_reading_finished", {
    readingId: "b1",
    status: "error"
  });
});

it("keeps a waiting browser reading out of desktop audio completion", async () => {
  await listeners.get("synthesis-state-change")!({ payload: true });
  await sendAudio(0);
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "waiting" } });
  await sendAudio(1);
  audio.onended?.(new Event("ended"));
  await vi.waitFor(() => expect(playSpy).toHaveBeenCalledTimes(2));
  audio.onended?.(new Event("ended"));
  expect(tauriCore.invoke).not.toHaveBeenCalledWith(
    "browser_reading_finished",
    expect.objectContaining({ readingId: "waiting" })
  );
  expect(tauriCore.invoke).not.toHaveBeenCalledWith(
    "browser_reading_progress",
    expect.objectContaining({ readingId: "waiting" })
  );
  await listeners.get("synthesis-state-change")!({ payload: true });
  await sendAudio(0, "waiting");
  await listeners.get("synthesis-state-change")!({ payload: false });
  audio.onended?.(new Event("ended"));
  expect(tauriCore.invoke).toHaveBeenCalledWith("browser_reading_finished", {
    readingId: "waiting",
    status: "completed"
  });
});

it("preserves a browser fragment's owner while it waits behind desktop playback", async () => {
  await listeners.get("synthesis-state-change")!({ payload: true });
  await sendAudio(0);
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "buffered" } });
  await sendAudio(0, "buffered");
  await sendAudio(1, "buffered");
  expect(playSpy).toHaveBeenCalledTimes(1);
  expect(tauriCore.invoke).not.toHaveBeenCalledWith(
    "browser_reading_progress",
    expect.objectContaining({ readingId: "buffered" })
  );
  audio.onended?.(new Event("ended"));
  await vi.waitFor(() => expect(playSpy).toHaveBeenCalledTimes(2));
  audio.onended?.(new Event("ended"));
  await vi.waitFor(() => expect(playSpy).toHaveBeenCalledTimes(3));
  audio.onended?.(new Event("ended"));
  expect(tauriCore.invoke).toHaveBeenCalledWith("browser_reading_finished", {
    readingId: "buffered",
    status: "completed"
  });
});

it("reports browser PCM progress only when the browser-owned source is audible", async () => {
  vi.useFakeTimers();
  vi.stubGlobal(
    "AudioContext",
    class {
      state = "running";
      currentTime = 0;
      destination = {};
      createGain = () => ({ gain: { value: 1 }, connect() {}, disconnect() {} });
      close = vi.fn();
    }
  );
  vi.spyOn(PcmStreamScheduler.prototype, "handleChunk").mockImplementation(() => {});
  const position = vi.spyOn(PcmStreamScheduler.prototype, "getPlaybackPosition");
  position.mockReturnValue({ fragmentIndex: 0, positionMs: 100, browserReadingId: null });
  await listeners.get("synthesis-state-change")!({ payload: true });
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "waiting-pcm" } });
  const chunk = {
    audio_base64: "YQ==",
    sample_rate: 24000,
    channels: 1,
    bits_per_sample: 16,
    fragment_index: 0,
    fragment_total: 1,
    is_final: false,
    text: "desktop"
  };
  playbackStore.handleStreamChunk(chunk);
  playbackStore.handleStreamChunk({ ...chunk, browser_reading_id: "waiting-pcm", text: "browser" });
  await vi.advanceTimersByTimeAsync(500);
  expect(tauriCore.invoke).not.toHaveBeenCalledWith(
    "browser_reading_progress",
    expect.objectContaining({ readingId: "waiting-pcm" })
  );
  position.mockReturnValue({ fragmentIndex: 0, positionMs: 200, browserReadingId: "waiting-pcm" });
  await vi.advanceTimersByTimeAsync(25);
  expect(tauriCore.invoke).toHaveBeenCalledWith("browser_reading_progress", {
    readingId: "waiting-pcm",
    fragmentIndex: 0,
    positionMs: 200,
    paused: false
  });
  playbackStore.handleStop();
});

it("reports a drained browser reading as completed", async () => {
  await listeners.get("synthesis-state-change")!({ payload: true });
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "b1" } });
  await sendAudio(0, "b1");
  await listeners.get("synthesis-state-change")!({ payload: false });
  audio.onended?.(new Event("ended"));
  expect(tauriCore.invoke).toHaveBeenCalledWith("browser_reading_finished", {
    readingId: "b1",
    status: "completed"
  });
});

it("releases a stopped browser reading as cancelled", async () => {
  await listeners.get("synthesis-state-change")!({ payload: true });
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "b1" } });
  await sendAudio(0, "b1");
  playbackStore.handleStop();
  expect(tauriCore.invoke).toHaveBeenCalledWith("browser_reading_finished", {
    readingId: "b1",
    status: "cancelled"
  });
});

it("leaves silent browser cancellation to the explicit backend stop command", async () => {
  await listeners.get("synthesis-state-change")!({ payload: true });
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "b2" } });
  playbackStore.handleStop();
  expect(tauriCore.invoke).not.toHaveBeenCalledWith(
    "browser_reading_finished",
    expect.objectContaining({ readingId: "b2" })
  );
});

it("ignores the previous reading's teardown while a new browser reading is silent", async () => {
  // The replaced reading's teardown emits all three events before the new
  // reading has produced audio. None may complete or stop the new session.
  await listeners.get("synthesis-state-change")!({ payload: true });
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "b3" } });
  await listeners.get("synthesis-state-change")!({ payload: false });
  await listeners.get("synthesis-aborted")!({ payload: null });
  await listeners.get("playback-stop")!({ payload: null });
  expect(tauriCore.invoke).not.toHaveBeenCalledWith(
    "browser_reading_finished",
    expect.objectContaining({ readingId: "b3" })
  );
  // The new reading still plays its first fragment afterwards.
  await sendAudio(0);
  expect(playbackStore.isPlaying).toBe(true);
});

it("ignores a terminal event for a superseded browser reading", async () => {
  await listeners.get("synthesis-state-change")!({ payload: true });
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "old" } });
  await listeners.get("browser-reading")!({ payload: { active: false, reading_id: "stale" } });
  await listeners.get("browser-reading")!({ payload: { active: true, reading_id: "new" } });
  await sendAudio(0, "new");
  await listeners.get("synthesis-state-change")!({ payload: false });
  audio.onended?.(new Event("ended"));
  expect(tauriCore.invoke).toHaveBeenCalledWith("browser_reading_finished", {
    readingId: "new",
    status: "completed"
  });
});

it.each(["decode", "play"])(
  "releases the queue after a %s failure so history can retry",
  async (failure) => {
    if (failure === "decode") decode.mockRejectedValueOnce(new Error("broken audio"));
    else playSpy.mockRejectedValueOnce(new Error("play rejected"));
    await sendAudio();
    expect(playbackStore.error).toContain("Audio playback failed");
    expect(playbackStore.isLoadingAudio).toBe(false);
    expect(playbackStore.isPlaying).toBe(false);
    await sendAudio();
    expect(decode).toHaveBeenCalledTimes(2);
    expect(playbackStore.isPlaying).toBe(true);
    expect(playbackStore.error).toBeNull();
  }
);

it("does not restart a stopped reading when its pending decode finishes", async () => {
  const pending = Promise.withResolvers<{ duration: number }>();
  decode.mockReturnValueOnce(pending.promise);
  const first = sendAudio(0);
  expect(playbackStore.isLoadingAudio).toBe(true);
  playbackStore.handleStop();
  pending.resolve({ duration: 1 });
  await first;
  expect(playSpy).not.toHaveBeenCalled();
  expect(playbackStore.isLoadingAudio).toBe(false);
  await sendAudio(0);
  expect(playSpy).toHaveBeenCalledTimes(1);
});

it("plays grouped fragments in order and retains the reading for Replay", async () => {
  await sendAudio(0);
  await sendAudio(1);
  expect(playSpy).toHaveBeenCalledTimes(1);
  expect(playbackStore.currentFragmentIndex).toBe(0);
  audio.dispatchEvent(new Event("ended"));
  await vi.waitFor(() => expect(playSpy).toHaveBeenCalledTimes(2));
  expect(playbackStore.currentFragmentIndex).toBe(1);
  audio.dispatchEvent(new Event("ended"));
  expect(playbackStore.isPlaying).toBe(false);
  expect(playbackStore.historyReadingId).toBe("batch:reading");
});

it("requests the whole-reading cache for Replay", async () => {
  const replay = vi.fn().mockResolvedValueOnce(undefined);
  vi.stubGlobal("__TAURI_INTERNALS__", { invoke: replay });
  await playbackStore.handleReplay();
  expect(replay).toHaveBeenCalledWith("replay_cached", {}, undefined);
  expect(playSpy).not.toHaveBeenCalled();
});

it("does not replay a partial local fragment when the backend cache is unavailable", async () => {
  vi.stubGlobal("__TAURI_INTERNALS__", {
    invoke: vi.fn().mockRejectedValueOnce(new Error("No cached audio to replay"))
  });
  await playbackStore.handleReplay();
  expect(playSpy).not.toHaveBeenCalled();
  expect(playbackStore.error).toContain("No cached audio to replay");
});

it("publishes captions from the audio clock and audible fragment, then stops publishing on Stop", async () => {
  vi.useFakeTimers();
  vi.spyOn(audio, "readyState", "get").mockReturnValue(4);
  vi.spyOn(audio, "duration", "get").mockReturnValue(1);
  await sendAudio(0);
  await sendAudio(1);
  audio.currentTime = 0.6;
  await vi.advanceTimersByTimeAsync(80);
  expect(emitTo).toHaveBeenLastCalledWith(
    "hud",
    "hud:caption",
    expect.objectContaining({ text: "Part 0", position_ms: 600, duration_ms: 1000, active: true })
  );
  audio.dispatchEvent(new Event("pause"));
  await vi.advanceTimersByTimeAsync(800);
  expect(emitTo).toHaveBeenLastCalledWith(
    "hud",
    "hud:caption",
    expect.objectContaining({ position_ms: 600, paused: true })
  );
  playbackStore.handleStop();
  vi.mocked(emitTo).mockClear();
  await vi.advanceTimersByTimeAsync(800);
  expect(emitTo).not.toHaveBeenCalled();
});

it("delays element captions by the AudioContext output latency in media time", async () => {
  vi.useFakeTimers();
  vi.stubGlobal(
    "AudioContext",
    class {
      state = "running";
      outputLatency = 0.2;
      decodeAudioData = decode;
      close = vi.fn();
    }
  );
  vi.spyOn(audio, "readyState", "get").mockReturnValue(4);
  vi.spyOn(audio, "duration", "get").mockReturnValue(1);
  await sendAudio(0);
  audio.playbackRate = 1.5;
  audio.currentTime = 0.6;
  await vi.advanceTimersByTimeAsync(25);
  expect(emitTo).toHaveBeenLastCalledWith(
    "hud",
    "hud:caption",
    expect.objectContaining({ position_ms: expect.closeTo(300) })
  );
  audio.currentTime = 0.1;
  await vi.advanceTimersByTimeAsync(25);
  expect(emitTo).toHaveBeenLastCalledWith(
    "hud",
    "hud:caption",
    expect.objectContaining({ position_ms: 0 })
  );
});

// The rendered blob carries native duration - pitch shifting no longer resamples
// it - so the media clock is already caption time, with no correction factor.
it.each([
  [0.5, 500],
  [Number.NaN, 1000]
])(
  "replays native timings through pitch, speed, and pause (duration %s)",
  async (duration, expectedDurationMs) => {
    vi.useFakeTimers();
    vi.spyOn(audio, "readyState", "get").mockReturnValue(4);
    vi.spyOn(audio, "duration", "get").mockReturnValue(duration);
    const captions = {
      text: "Hello world",
      words: [
        { text_start: 0, text_end: 5, start_ms: 100, end_ms: 200 },
        { text_start: 6, text_end: 11, start_ms: 600, end_ms: 800 }
      ]
    };
    playbackStore.syncPlaybackConfig(100, 1.5, 2);
    await listeners.get("audio-fragment-ready")!({
      payload: {
        audio_base64: "YQ==",
        fragment_index: 0,
        fragment_total: 1,
        is_final: true,
        text: "Hello world",
        captions
      }
    });
    audio.currentTime = 0.3;
    // Changing the configured pitch cannot alter the already-rendered media clock.
    playbackStore.syncPlaybackConfig(100, 2, 0.5);
    await vi.advanceTimersByTimeAsync(25);
    expect(emitTo).toHaveBeenLastCalledWith(
      "hud",
      "hud:caption",
      expect.objectContaining({
        captions,
        position_ms: 300,
        duration_ms: expectedDurationMs
      })
    );
    audio.dispatchEvent(new Event("pause"));
    await vi.advanceTimersByTimeAsync(100);
    expect(emitTo).toHaveBeenLastCalledWith(
      "hud",
      "hud:caption",
      expect.objectContaining({ captions, position_ms: 300, paused: true })
    );
    playbackStore.syncPlaybackConfig(100, 1, 1);
  }
);

it("keeps streaming captions on the audible fragment and rejects chunks after Stop", async () => {
  vi.useFakeTimers();
  vi.stubGlobal(
    "AudioContext",
    class {
      state = "running";
      currentTime = 0;
      destination = {};
      createGain = () => ({ gain: { value: 1 }, connect() {}, disconnect() {} });
      resume = vi.fn();
      suspend = vi.fn(() => {
        this.state = "suspended";
      });
      close = vi.fn();
    }
  );
  const receive = vi
    .spyOn(PcmStreamScheduler.prototype, "handleChunk")
    .mockImplementation(() => {});
  vi.spyOn(PcmStreamScheduler.prototype, "getPlaybackPosition").mockReturnValue({
    fragmentIndex: 0,
    positionMs: 700
  });
  await listeners.get("synthesis-state-change")!({ payload: true });
  const captions = {
    text: "first",
    words: [{ text_start: 0, text_end: 5, start_ms: 650, end_ms: 800 }]
  };
  const payload = {
    audio_base64: "",
    sample_rate: 24000,
    channels: 1,
    bits_per_sample: 16,
    fragment_index: 0,
    fragment_total: 2,
    is_final: false,
    text: "first",
    captions
  };
  playbackStore.handleStreamChunk(payload);
  playbackStore.handleTogglePause();
  playbackStore.handleStreamChunk({
    ...payload,
    fragment_index: 1,
    text: "second",
    captions: { text: "second", words: [] }
  });
  expect(playbackStore.isPaused).toBe(true);
  expect(emitTo).toHaveBeenLastCalledWith(
    "hud",
    "hud:caption",
    expect.objectContaining({ text: "first", captions, position_ms: 700, paused: true })
  );
  const complete = vi
    .spyOn(PcmStreamScheduler.prototype, "markQueueComplete")
    .mockImplementation(() => {});
  for (const terminal of ["pagination:complete", "pagination:stopped", "synthesis-state-change"]) {
    await listeners.get(terminal)!({ payload: false });
    expect(playbackStore.isPlaying).toBe(true);
  }
  expect(complete).toHaveBeenCalledTimes(3);
  playbackStore.handleStop();
  receive.mockClear();
  vi.mocked(emitTo).mockClear();
  playbackStore.handleStreamChunk(payload);
  await vi.advanceTimersByTimeAsync(500);
  expect(receive).not.toHaveBeenCalled();
  expect(emitTo).not.toHaveBeenCalled();
});
