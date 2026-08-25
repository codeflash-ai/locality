import { describe, expect, it } from "vitest";
import {
  googleDocsMountedMountId,
  googleDocsPickerCommand,
  googleDocsSelectionNeededForMount,
} from "./App";

describe("Google Docs Picker", () => {
  it("requests selection through the loopback browser command", () => {
    expect(googleDocsPickerCommand()).toBe("choose_google_docs_in_browser");
  });

  it("reopens Picker before creating an unselected Google Docs mount", () => {
    expect(googleDocsSelectionNeededForMount("google-docs", [])).toBe(true);
    expect(googleDocsSelectionNeededForMount("google-docs", ["doc-1"])).toBe(false);
    expect(googleDocsSelectionNeededForMount("notion", [])).toBe(false);
  });
});

it("uses the actual mounted Google Docs ID when reconfiguring", () => {
  expect(googleDocsMountedMountId({ mounts: [{ connector: "google-docs", mountId: "docs-team-a" }] } as any))
    .toBe("docs-team-a");
  expect(googleDocsMountedMountId({ mounts: [], mount: { connector: "notion", status: "ready" } } as any)).toBeNull();
});
