import { describe, expect, it } from "vitest";
import { shouldAutoCreateMount } from "./onboarding-flow";

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
      }),
    ).toBe(false);
  });
});
