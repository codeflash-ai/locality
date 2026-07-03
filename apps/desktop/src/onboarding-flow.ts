type AutoCreateMountOptions = {
  step: number;
  connectionReady: boolean;
  mountMissing: boolean;
  mounting: boolean;
  hasMountError: boolean;
  mountPath: string;
};

export function shouldAutoCreateMount({
  step,
  connectionReady,
  mountMissing,
  mounting,
  hasMountError,
  mountPath,
}: AutoCreateMountOptions) {
  return (
    step === 4 &&
    connectionReady &&
    mountMissing &&
    !mounting &&
    !hasMountError &&
    mountPath.trim().length > 0
  );
}
