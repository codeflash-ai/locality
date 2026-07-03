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

export function mountRecoveryEnabled(error: MountSetupError | null) {
  return error?.kind === "path-validation";
}
