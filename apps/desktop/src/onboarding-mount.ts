export type WorkspaceMountOnboardingState =
  | "created"
  | "approval_required"
  | "waiting_for_cloudstorage_root"
  | "failed";

export type WorkspaceMountOnboardingPrimaryAction =
  | "allow_in_macos"
  | "check_again"
  | "retry_setup";

export type WorkspaceMountOnboardingLaunchStrategy =
  | "open_finder"
  | "instructions_only"
  | "none";

export type WorkspaceMountOnboardingReport = {
  state: WorkspaceMountOnboardingState;
  message: string;
  primaryAction: WorkspaceMountOnboardingPrimaryAction;
  launchStrategy: WorkspaceMountOnboardingLaunchStrategy;
};

export function mountOnboardingPrimaryLabel(
  report: WorkspaceMountOnboardingReport | null,
  busy: boolean,
) {
  if (busy && report?.primaryAction === "allow_in_macos") {
    return "Opening Finder";
  }
  if (busy) {
    return "Checking setup";
  }
  switch (report?.primaryAction) {
    case "allow_in_macos":
      return "Allow in macOS";
    case "check_again":
      return "Check again";
    case "retry_setup":
      return "Retry setup";
    default:
      return "Preparing local folder";
  }
}

export function mountOnboardingNeedsInstructions(
  report: WorkspaceMountOnboardingReport | null,
) {
  return report?.launchStrategy === "instructions_only";
}

export function mountOnboardingNextAction(
  report: WorkspaceMountOnboardingReport | null,
): "start" | "allow_in_macos" | "check_again" {
  switch (report?.primaryAction) {
    case "allow_in_macos":
      return "allow_in_macos";
    case "check_again":
      return "check_again";
    default:
      return "start";
  }
}

export function failedMountOnboardingReport(
  message: string,
): WorkspaceMountOnboardingReport {
  return {
    state: "failed",
    message,
    primaryAction: "retry_setup",
    launchStrategy: "none",
  };
}
