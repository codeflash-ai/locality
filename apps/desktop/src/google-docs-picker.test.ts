import { describe, expect, it } from "vitest";
import { googleDocsPickerOptions } from "./App";

describe("Google Docs Picker options", () => {
  it("limits multi-select to native Google Docs files", () => {
    expect(googleDocsPickerOptions()).toEqual({
      mimeTypes: "application/vnd.google-apps.document",
      multiSelect: true,
    });
  });
});
