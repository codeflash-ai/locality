export type SourceSetupState = "idle" | "connecting" | "creating" | "changing" | "success" | "error";
export type SourceConnectorId = "notion" | "google-docs" | "gmail" | "granola";

export function sourceSetupIsBusy(state: SourceSetupState): boolean {
  return state === "connecting" || state === "creating" || state === "changing";
}

export function sourceSetupIsActiveConnector(
  state: SourceSetupState,
  activeConnector: SourceConnectorId | null,
  connector: SourceConnectorId,
): boolean {
  return sourceSetupIsBusy(state) && activeConnector === connector;
}

export function sourceSetupProgressLabel(state: SourceSetupState, mounted: boolean): string {
  if (state === "changing") {
    return "Updating access";
  }
  if (mounted) {
    return "Finishing setup";
  }
  if (state === "creating") {
    return "Mounting";
  }
  if (state === "connecting") {
    return "Connecting";
  }
  return "";
}
