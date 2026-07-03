export type MountSetupErrorKind = "file-provider-disabled" | "path-validation" | "generic";

export type MountSetupError = {
  kind: MountSetupErrorKind;
  message: string;
};

export function classifyMountSetupError(message: string): MountSetupError {
  if (message.includes("registered but not enabled")) {
    return { kind: "file-provider-disabled", message };
  }
  if (
    message.includes("Choose a folder") ||
    message.includes("Choose a CloudStorage folder") ||
    message.includes("Choose a mount point inside the Locality File Provider root") ||
    message.includes("not a file") ||
    message.includes("read-only") ||
    message.includes("No existing parent folder") ||
    message.includes("Mount parent is not a folder") ||
    message.includes("Could not inspect parent folder")
  ) {
    return { kind: "path-validation", message };
  }
  return { kind: "generic", message };
}
