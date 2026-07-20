export type WorkspaceMountOnboardingState =
  | "created"
  | "needs_finder_enable"
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

export function mountOnboardingHeadline(
  report: WorkspaceMountOnboardingReport | null,
) {
  switch (report?.state) {
    case "needs_finder_enable":
      return "Enable Locality in Finder";
    case "waiting_for_cloudstorage_root":
      return "Waiting for the Locality folder to appear.";
    case "failed":
      return "Folder setup needs attention.";
    default:
      return "Creating your local folder.";
  }
}

export function mountOnboardingNeedsInstructions(
  report: WorkspaceMountOnboardingReport | null,
) {
  return mountOnboardingInstructions(report) !== null;
}

export function mountOnboardingInstructions(
  report: WorkspaceMountOnboardingReport | null,
) {
  if (report?.state !== "needs_finder_enable" || report.launchStrategy !== "instructions_only") {
    return null;
  }
  return (
    "Finder is open to Locality. Click Enable there; this screen will continue automatically."
  );
}

export function mountOnboardingSupplementaryNote(
  report: WorkspaceMountOnboardingReport | null,
) {
  if (report?.state !== "waiting_for_cloudstorage_root") {
    return null;
  }
  return "Locality is waiting for macOS to create the CloudStorage folder before the final onboarding step can continue.";
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
