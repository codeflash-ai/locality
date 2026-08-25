import { describe, expect, it } from "vitest";
import { googleDocsPickerOptions, loadGooglePicker } from "./App";

describe("Google Docs Picker options", () => {
  it("limits multi-select to native Google Docs files", () => {
    expect(googleDocsPickerOptions()).toEqual({
      mimeTypes: "application/vnd.google-apps.document",
      multiSelect: true,
    });
  });
});

it("rejects and permits retry when the Google script does not expose gapi", async () => {
  const scripts: Array<{ onload?: () => void; onerror?: () => void }> = [];
  (globalThis as any).window = {};
  (globalThis as any).document = {
    createElement: () => ({}),
    head: { appendChild: (script: any) => scripts.push(script) },
  };

  const first = loadGooglePicker();
  scripts[0].onload?.();
  await expect(first).rejects.toThrow("did not load correctly");

  const second = loadGooglePicker();
  scripts[1].onerror?.();
  await expect(second).rejects.toThrow("Could not load Google Picker");
});
