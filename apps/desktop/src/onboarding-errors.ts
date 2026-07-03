export type MountSetupErrorKind = "file-provider-disabled" | "generic";

export type MountSetupError = {
  kind: MountSetupErrorKind;
  message: string;
};

export function classifyMountSetupError(message: string): MountSetupError {
  if (message.includes("registered but not enabled")) {
    return { kind: "file-provider-disabled", message };
  }
  return { kind: "generic", message };
}
