import { expect, it, vi } from "vite-plus/test";
import { fireEvent, render } from "@testing-library/svelte";
import { waitForI18nReady } from "$lib/i18n";
import PlaybackControls from "./playback-controls.svelte";

it("uses one button for play, stop, replay, and cancelling synthesis", async () => {
  await waitForI18nReady();
  const onPlay = vi.fn();
  const onStop = vi.fn();
  const onAbort = vi.fn();
  const onTogglePause = vi.fn();
  const view = render(PlaybackControls, {
    isPlaying: false,
    isPaused: false,
    isSynthesizing: false,
    playMode: "play",
    onPlay,
    onStop,
    onAbort,
    onTogglePause
  });
  await fireEvent.click(view.getByRole("button", { name: "Play" }));
  expect(onPlay).toHaveBeenCalledTimes(1);
  await view.rerender({ isPlaying: true });
  expect(view.getAllByRole("button")).toHaveLength(2);
  await fireEvent.click(view.getByRole("button", { name: "Stop" }));
  expect(onStop).toHaveBeenCalledTimes(1);
  await fireEvent.click(view.getByRole("button", { name: "Pause" }));
  expect(onTogglePause).toHaveBeenCalledTimes(1);
  await view.rerender({ isPlaying: true, isPaused: true });
  await fireEvent.click(view.getByRole("button", { name: "Resume" }));
  expect(onTogglePause).toHaveBeenCalledTimes(2);
  await view.rerender({ isPlaying: false, playMode: "replay" });
  await fireEvent.click(view.getByRole("button", { name: "Replay" }));
  expect(onPlay).toHaveBeenCalledTimes(2);
  await view.rerender({ isSynthesizing: true, playMode: "disabled" });
  await fireEvent.click(view.getByRole("button", { name: "Stop" }));
  expect(onAbort).toHaveBeenCalledTimes(1);
});
