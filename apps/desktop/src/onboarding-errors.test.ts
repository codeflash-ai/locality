import { describe, expect, it } from "vitest";
import { classifyMountSetupError } from "./onboarding-errors";

describe("classifyMountSetupError", () => {
  it("detects disabled macOS File Provider setup", () => {
    expect(
      classifyMountSetupError(
        "Could not open macOS File Provider domain `loc`: The Locality File Provider is registered but not enabled. Enable Locality in Finder or System Settings, then try again.",
      ),
    ).toMatchObject({ kind: "file-provider-disabled" });
  });

  it("keeps unrelated mount errors generic", () => {
    expect(classifyMountSetupError("Could not load the top-level Notion folder")).toMatchObject({
      kind: "generic",
    });
  });
});
