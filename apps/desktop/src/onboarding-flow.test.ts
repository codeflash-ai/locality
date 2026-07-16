import { describe, expect, it } from "vitest";
import {
  initialOnboardingStepForRoute,
  mountProviderSetupRequiredForOnboarding,
  mountRecoveryEnabled,
  nextOnboardingStepAfterInitialStepChange,
  nextOnboardingStepForReadySnapshot,
  shouldAutoCreateMount,
} from "./onboarding-flow";

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

describe("nextOnboardingStepForReadySnapshot", () => {
  it("keeps existing macOS File Provider mounts on the approval step until enabled", () => {
    expect(
      nextOnboardingStepForReadySnapshot({
        currentStep: 5,
        mountMissing: false,
        connectorSkipsMountStep: false,
        providerApprovalRequired: true,
      }),
    ).toBe(4);
  });
});

describe("initialOnboardingStepForRoute", () => {
  it("starts forced onboarding at the macOS approval step when provider approval is pending", () => {
    expect(
      initialOnboardingStepForRoute({
        route: "#onboarding",
        providerApprovalRequired: true,
      }),
    ).toBe(4);
  });
});

describe("nextOnboardingStepAfterInitialStepChange", () => {
  it("moves an already mounted onboarding view to the latest parent-selected step", () => {
    expect(
      nextOnboardingStepAfterInitialStepChange({
        currentStep: 3,
        initialStep: 4,
      }),
    ).toBe(4);
  });
});

describe("mountProviderSetupRequiredForOnboarding", () => {
  it("requires the macOS setup step while the File Provider domain is unregistered", () => {
    expect(
      mountProviderSetupRequiredForOnboarding({
        mountStatus: "provider_unregistered",
        providerState: "running",
        providerRegistered: false,
      }),
    ).toBe(true);
  });

  it("requires the macOS setup step while provider status cannot be inspected", () => {
    expect(
      mountProviderSetupRequiredForOnboarding({
        mountStatus: "provider_error",
        providerState: "error",
        providerRegistered: null,
      }),
    ).toBe(true);
  });
});
