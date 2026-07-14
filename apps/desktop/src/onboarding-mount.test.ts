import { describe, expect, it } from "vitest";
import * as onboardingMount from "./onboarding-mount";
import {
  failedMountOnboardingReport,
  mountOnboardingHeadline,
  mountOnboardingNeedsInstructions,
  mountOnboardingNextAction,
  mountOnboardingPrimaryLabel,
  mountOnboardingSupplementaryNote,
  type WorkspaceMountOnboardingReport,
} from "./onboarding-mount";

const mountOnboardingInstructions = (
  onboardingMount as {
    mountOnboardingInstructions?: (
      report: WorkspaceMountOnboardingReport | null,
    ) => string | null;
  }
).mountOnboardingInstructions;

function report(
  overrides: Partial<WorkspaceMountOnboardingReport>,
): WorkspaceMountOnboardingReport {
  return {
    state: "approval_required",
    message: "Enable Locality in Finder, then return here and click Check again.",
    primaryAction: "allow_in_macos",
    launchStrategy: "instructions_only",
    ...overrides,
  };
}

describe("mount onboarding helpers", () => {
  it("labels the approval CTA from the backend action", () => {
    expect(mountOnboardingPrimaryLabel(report({ primaryAction: "allow_in_macos" }), false)).toBe(
      "Allow in macOS",
    );
    expect(mountOnboardingPrimaryLabel(report({ primaryAction: "check_again" }), false)).toBe(
      "Check again",
    );
    expect(mountOnboardingPrimaryLabel(report({ primaryAction: "retry_setup" }), false)).toBe(
      "Retry setup",
    );
  });

  it("labels and describes the native macOS approval prompt", () => {
    expect(mountOnboardingPrimaryLabel(report({ primaryAction: "allow_in_macos" }), true)).toBe(
      "Opening macOS prompt",
    );
    expect(mountOnboardingHeadline(report({ state: "approval_required" }))).toBe(
      "Approve the macOS Start Syncing prompt.",
    );
  });

  it("shows instructions only when Finder could not be opened directly", () => {
    expect(mountOnboardingNeedsInstructions(report({ launchStrategy: "instructions_only" }))).toBe(
      true,
    );
    expect(mountOnboardingNeedsInstructions(report({ launchStrategy: "open_finder" }))).toBe(
      false,
    );
  });

  it("does not show approval instructions while waiting for the CloudStorage root", () => {
    expect(
      mountOnboardingNeedsInstructions(
        report({
          state: "waiting_for_cloudstorage_root",
          primaryAction: "check_again",
          launchStrategy: "instructions_only",
        }),
      ),
    ).toBe(false);
  });

  it("explains that OK in the native prompt enables the File Provider location", () => {
    const instructions =
      mountOnboardingInstructions?.(report({ launchStrategy: "instructions_only" })) ?? null;

    expect(instructions).toBe(
      `Click OK in the macOS "Start Syncing" prompt, then Locality will check the folder again. If you clicked Don't allow, choose Allow in macOS to try again.`,
    );
    expect(instructions).not.toContain("enable the File Provider");
    expect(instructions).not.toContain("System Settings");
  });

  it("maps backend states to the step 4 headline", () => {
    expect(mountOnboardingHeadline(report({ state: "approval_required" }))).toBe(
      "Approve the macOS Start Syncing prompt.",
    );
    expect(mountOnboardingHeadline(report({ state: "waiting_for_cloudstorage_root" }))).toBe(
      "Waiting for the Locality folder to appear.",
    );
    expect(mountOnboardingHeadline(report({ state: "failed" }))).toBe(
      "Folder setup needs attention.",
    );
    expect(mountOnboardingHeadline(null)).toBe("Creating your local folder.");
  });

  it("shows the CloudStorage waiting note only for the waiting-root state", () => {
    expect(
      mountOnboardingSupplementaryNote(report({ state: "waiting_for_cloudstorage_root" })),
    ).toContain("CloudStorage");
    expect(mountOnboardingSupplementaryNote(report({ state: "approval_required" }))).toBeNull();
    expect(mountOnboardingSupplementaryNote(null)).toBeNull();
  });

  it("maps the backend report to the next onboarding command action", () => {
    expect(mountOnboardingNextAction(report({ primaryAction: "allow_in_macos" }))).toBe(
      "allow_in_macos",
    );
    expect(mountOnboardingNextAction(report({ primaryAction: "check_again" }))).toBe(
      "check_again",
    );
    expect(mountOnboardingNextAction(report({ primaryAction: "retry_setup" }))).toBe("start");
  });

  it("wraps generic failures into a retryable onboarding report", () => {
    expect(failedMountOnboardingReport("Could not load the top-level Notion folder")).toEqual({
      state: "failed",
      message: "Could not load the top-level Notion folder",
      primaryAction: "retry_setup",
      launchStrategy: "none",
    });
  });
});
