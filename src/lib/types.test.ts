import { expect, it } from "vite-plus/test";
import { isRetriggerMode, RETRIGGER_MODE_OPTIONS, RETRIGGER_MODES } from "$lib/types";

// The backend `RetriggerMode` enum is serde-renamed to lowercase and accepts
// only `interrupt` and `queue`. A dropdown value outside that set makes
// `set_config` fail with "unknown variant", which also blocks saving unrelated
// settings. These tests lock the UI options to the backend contract.
it("offers only retrigger values the backend enum accepts", () => {
  expect(RETRIGGER_MODE_OPTIONS.map((option) => option.value)).toEqual(["interrupt", "queue"]);
});

it("recognises every backend retrigger value and rejects others", () => {
  for (const mode of RETRIGGER_MODES) expect(isRetriggerMode(mode)).toBe(true);
  expect(isRetriggerMode("stop")).toBe(false);
  expect(isRetriggerMode("ignore")).toBe(false);
  expect(isRetriggerMode("")).toBe(false);
});
