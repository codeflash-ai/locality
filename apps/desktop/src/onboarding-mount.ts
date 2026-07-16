export type WorkspaceMountOnboardingState =
  | "created"
  | "approval_required"
  | "waiting_for_cloudstorage_root"
  | "failed";

export type WorkspaceMountOnboardingPrimaryAction =
  | "allow_in_macos"
  | "check_again"
  | "retry_setup";

export type WorkspaceMountOnboardingCommandAction =
  | "start"
  | "restore"
  | "allow_in_macos"
  | "check_again";

export type WorkspaceMountOnboardingLaunchStrategy =
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
    return "Waiting for macOS";
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
      return "Allow Locality to sync.";
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
    "Click OK in the macOS \"Start Syncing\" prompt. If macOS opens Finder instead, " +
    "click Enable in the Locality folder, then return here and click Allow in macOS."
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

export function providerSetupMountOnboardingReport(
  message: string | null | undefined,
): WorkspaceMountOnboardingReport {
  return {
    state: "approval_required",
    message:
      message?.trim() ||
      "Click OK in the macOS \"Start Syncing\" prompt. Locality will continue once macOS enables the CloudStorage folder.",
    primaryAction: "allow_in_macos",
    launchStrategy: "instructions_only",
  };
}

export function mountOnboardingNextAction(
  report: WorkspaceMountOnboardingReport | null,
): WorkspaceMountOnboardingCommandAction {
  switch (report?.primaryAction) {
    case "allow_in_macos":
      return "allow_in_macos";
    case "check_again":
      return "check_again";
    default:
      return "start";
  }
}

export function automaticMountOnboardingAction(): WorkspaceMountOnboardingCommandAction {
  return "restore";
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
