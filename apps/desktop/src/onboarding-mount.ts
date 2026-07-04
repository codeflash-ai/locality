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

export function mountOnboardingHeadline(
  report: WorkspaceMountOnboardingReport | null,
) {
  switch (report?.state) {
    case "approval_required":
      return "Allow Locality in Finder.";
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
  if (report?.state !== "approval_required" || report.launchStrategy !== "instructions_only") {
    return null;
  }
  return (
    "Open Finder, choose Locality under Locations, enable the File Provider, then return here " +
    "and click Check again. If Finder does not show Locality, open System Settings, go to " +
    "Privacy & Security, then enable Locality under Extensions or File Providers."
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

export function mountOnboardingShowsExtensionBrowserSpike(
  report: WorkspaceMountOnboardingReport | null,
  options: { devMode: boolean; appStoreDistribution: boolean },
) {
  return (
    options.devMode &&
    !options.appStoreDistribution &&
    report?.state === "approval_required"
  );
}

export function mountOnboardingExtensionBrowserSpikeLabel(busy: boolean) {
  return busy ? "Opening Approval Window" : "Open Approval Window (Spike)";
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
