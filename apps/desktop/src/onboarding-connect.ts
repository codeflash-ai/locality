export type LoginLinkFlowMode = "copy-existing-link" | "start-without-browser";

export function loginLinkFlowMode({
  connectionReady,
  oauthInFlight,
  loginUrl,
}: {
  connectionReady: boolean;
  oauthInFlight: boolean;
  loginUrl: string;
}): LoginLinkFlowMode {
  if (!connectionReady && !oauthInFlight && loginUrl.trim() === "") {
    return "start-without-browser";
  }
  return "copy-existing-link";
}

export function copyLoginLinkDisabled({
  connectionReady,
  oauthInFlight,
}: {
  connectionReady: boolean;
  oauthInFlight: boolean;
}) {
  return connectionReady && !oauthInFlight;
}
