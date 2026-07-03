import { describe, expect, it } from "vitest";
import {
  failedMountOnboardingReport,
  mountOnboardingNeedsInstructions,
  mountOnboardingNextAction,
  mountOnboardingPrimaryLabel,
  type WorkspaceMountOnboardingReport,
} from "./onboarding-mount";

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

  it("switches to progress copy while the action is running", () => {
    expect(mountOnboardingPrimaryLabel(report({ primaryAction: "allow_in_macos" }), true)).toBe(
      "Opening Finder",
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
