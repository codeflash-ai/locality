import { describe, expect, it } from "vitest";
import { googleDocsMountedMountId, googleDocsPickerCommand } from "./App";

describe("Google Docs Picker", () => {
  it("requests selection through the loopback browser command", () => {
    expect(googleDocsPickerCommand()).toBe("choose_google_docs_in_browser");
  });
});

it("uses the actual mounted Google Docs ID when reconfiguring", () => {
  expect(googleDocsMountedMountId({ mounts: [{ connector: "google-docs", mountId: "docs-team-a" }] } as any))
    .toBe("docs-team-a");
  expect(googleDocsMountedMountId({ mounts: [], mount: { connector: "notion", status: "ready" } } as any)).toBeNull();
});
