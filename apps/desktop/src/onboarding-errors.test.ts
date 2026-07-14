import { describe, expect, it } from "vitest";
import { classifyMountSetupError } from "./onboarding-errors";

describe("classifyMountSetupError", () => {
  it("detects disabled macOS File Provider setup", () => {
    expect(
      classifyMountSetupError(
        'Could not open macOS File Provider domain `loc`: The Locality File Provider is registered but not enabled. Click OK in the macOS "Start Syncing" prompt, then try again.',
      ),
    ).toMatchObject({ kind: "file-provider-disabled" });
  });

  it("classifies path validation failures separately from generic mount errors", () => {
    expect(classifyMountSetupError("Choose a folder path, not a file: /tmp/notion")).toMatchObject({
      kind: "path-validation",
    });
    expect(classifyMountSetupError("Mount parent folder is read-only: /tmp")).toMatchObject({
      kind: "path-validation",
    });
    expect(classifyMountSetupError("Selected folder is read-only: /tmp/notion")).toMatchObject({
      kind: "path-validation",
    });
  });

  it("keeps unrelated mount errors generic", () => {
    expect(classifyMountSetupError("Could not load the top-level Notion folder")).toMatchObject({
      kind: "generic",
    });
  });
});
