export type SourceSetupState = "idle" | "connecting" | "creating" | "changing" | "success" | "error";
const SOURCE_CONNECTOR_IDS = ["notion", "google-docs", "gmail", "granola", "linear"] as const;
export type SourceConnectorId = (typeof SOURCE_CONNECTOR_IDS)[number];

export function sourceConnectorIds(): SourceConnectorId[] {
  return [...SOURCE_CONNECTOR_IDS];
}

export function sourceRequiresApiKey(connector: SourceConnectorId): boolean {
  return connector === "granola" || connector === "linear";
}

export function sourceSkipsManualMountStep(connector: SourceConnectorId): boolean {
  return connector !== "notion";
}

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
