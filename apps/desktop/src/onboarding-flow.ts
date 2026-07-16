import type { MountSetupError } from "./onboarding-errors";

type AutoCreateMountOptions = {
  step: number;
  connectionReady: boolean;
  mountMissing: boolean;
  mounting: boolean;
  hasMountError: boolean;
  mountPath: string;
  startRequested?: boolean;
};

export function shouldAutoCreateMount({
  step,
  connectionReady,
  mountMissing,
  mounting,
  hasMountError,
  mountPath,
  startRequested = false,
}: AutoCreateMountOptions) {
  return (
    step === 4 &&
    connectionReady &&
    mountMissing &&
    !mounting &&
    !hasMountError &&
    !startRequested &&
    mountPath.trim().length > 0
  );
}

type NextOnboardingStepOptions = {
  currentStep: number;
  mountMissing: boolean;
  connectorSkipsMountStep: boolean;
  providerApprovalRequired: boolean;
};

type InitialOnboardingStepOptions = {
  route: string;
  providerApprovalRequired: boolean;
};

export function initialOnboardingStepForRoute({
  route,
  providerApprovalRequired,
}: InitialOnboardingStepOptions) {
  if (providerApprovalRequired) {
    return 4;
  }
  if (route === "#onboarding-ready") {
    return 5;
  }
  return 1;
}

type InitialStepChangeOptions = {
  currentStep: number;
  initialStep: number;
};

export function nextOnboardingStepAfterInitialStepChange({
  currentStep,
  initialStep,
}: InitialStepChangeOptions) {
  return currentStep === initialStep ? currentStep : initialStep;
}

type MountProviderSetupOptions = {
  mountStatus: string;
  providerState?: string | null;
  providerRegistered?: boolean | null;
};

export function mountProviderSetupRequiredForOnboarding({
  mountStatus,
  providerState,
  providerRegistered,
}: MountProviderSetupOptions) {
  return (
    mountStatus === "provider_approval_required" ||
    mountStatus === "provider_unregistered" ||
    mountStatus === "provider_error" ||
    providerState === "approval_required" ||
    providerState === "error" ||
    providerRegistered === false
  );
}

export function nextOnboardingStepForReadySnapshot({
  currentStep,
  mountMissing,
  connectorSkipsMountStep,
  providerApprovalRequired,
}: NextOnboardingStepOptions) {
  if (providerApprovalRequired) {
    return 4;
  }
  if (mountMissing) {
    if (connectorSkipsMountStep) {
      return currentStep < 3 ? 3 : currentStep;
    }
    return currentStep < 4 ? 4 : currentStep;
  }
  return currentStep < 5 ? 5 : currentStep;
}

export function mountRecoveryEnabled(error: MountSetupError | null) {
  return error?.kind === "path-validation";
}
