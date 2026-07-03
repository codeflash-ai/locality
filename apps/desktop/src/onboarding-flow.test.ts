import { describe, expect, it } from "vitest";
import { mountRecoveryEnabled, shouldAutoCreateMount } from "./onboarding-flow";

describe("shouldAutoCreateMount", () => {
  it("starts automatic mount creation on the setup step when the workspace is connected", () => {
    expect(
      shouldAutoCreateMount({
        step: 4,
        connectionReady: true,
        mountMissing: true,
        mounting: false,
        hasMountError: false,
        mountPath: "~/Library/CloudStorage/Locality/notion",
        startRequested: false,
      }),
    ).toBe(true);
  });

  it("does not auto-retry after a mount error until the user asks", () => {
    expect(
      shouldAutoCreateMount({
        step: 4,
        connectionReady: true,
        mountMissing: true,
        mounting: false,
        hasMountError: true,
        mountPath: "~/Library/CloudStorage/Locality/notion",
        startRequested: false,
      }),
    ).toBe(false);
  });

  it("does not auto-start while a mount request is already in flight", () => {
    expect(
      shouldAutoCreateMount({
        step: 4,
        connectionReady: true,
        mountMissing: true,
        mounting: false,
        hasMountError: false,
        mountPath: "~/Library/CloudStorage/Locality/notion",
        startRequested: true,
      }),
    ).toBe(false);
  });

  it("requires the setup step, a missing mount, and a mount path", () => {
    expect(
      shouldAutoCreateMount({
        step: 3,
        connectionReady: true,
        mountMissing: true,
        mounting: false,
        hasMountError: false,
        mountPath: "~/Library/CloudStorage/Locality/notion",
        startRequested: false,
      }),
    ).toBe(false);
    expect(
      shouldAutoCreateMount({
        step: 4,
        connectionReady: true,
        mountMissing: false,
        mounting: false,
        hasMountError: false,
        mountPath: "~/Library/CloudStorage/Locality/notion",
        startRequested: false,
      }),
    ).toBe(false);
    expect(
      shouldAutoCreateMount({
        step: 4,
        connectionReady: true,
        mountMissing: true,
        mounting: false,
        hasMountError: false,
        mountPath: "   ",
        startRequested: false,
      }),
    ).toBe(false);
  });
});

describe("mountRecoveryEnabled", () => {
  it("shows chooser recovery for path validation failures", () => {
    expect(
      mountRecoveryEnabled({
        kind: "path-validation",
        message: "Choose a folder path, not a file: /tmp/notion",
      }),
    ).toBe(true);
    expect(
      mountRecoveryEnabled({
        kind: "path-validation",
        message: "Mount parent folder is read-only: /tmp",
      }),
    ).toBe(true);
  });

  it("keeps chooser recovery hidden for non-path failures", () => {
    expect(
      mountRecoveryEnabled({
        kind: "file-provider-disabled",
        message: "The Locality File Provider is registered but not enabled.",
      }),
    ).toBe(false);
    expect(
      mountRecoveryEnabled({
        kind: "generic",
        message: "Could not load the top-level Notion folder",
      }),
    ).toBe(false);
  });
});
