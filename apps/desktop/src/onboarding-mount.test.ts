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
    state: "needs_finder_enable",
    message: "In Finder, click Enable for Locality. Locality will continue automatically.",
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

  it("switches to Finder progress copy while the action is running", () => {
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

  it("explains automatic continuation without a confirmation click", () => {
    expect(
      mountOnboardingInstructions?.(report({ launchStrategy: "instructions_only" })) ?? null,
    ).toBe(
      "Finder is open to Locality. Click Enable there; this screen will continue automatically.",
    );
  });

  it("maps backend states to the step 4 headline", () => {
    expect(mountOnboardingHeadline(report({ state: "needs_finder_enable" }))).toBe(
      "Enable Locality in Finder",
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
    expect(mountOnboardingSupplementaryNote(report({ state: "needs_finder_enable" }))).toBeNull();
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
