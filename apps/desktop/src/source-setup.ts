import { classifyMountSetupError } from "./onboarding-errors";

export type SourceSetupState = "idle" | "connecting" | "creating" | "changing" | "success" | "error";
const SOURCE_CONNECTORS = ["notion", "google-docs", "google-calendar", "gmail", "granola", "linear", "slack"] as const;
export type SourceConnectorId = (typeof SOURCE_CONNECTORS)[number];
export type ApiKeySourceConnectorId = Extract<SourceConnectorId, "granola" | "linear">;
export type SourceMountRetryOutcome =
  | { kind: "retry" }
  | { kind: "success" | "error"; message: string };

type SourceConnectionLike = {
  connector: string;
  status: string;
};

type SourceMountLike = {
  connector: string;
  status?: string | null;
};

type SourceSnapshotLike = {
  connection?: SourceConnectionLike | null;
  connections?: SourceConnectionLike[] | null;
  mount?: SourceMountLike | null;
  mounts?: SourceMountLike[] | null;
};

export function sourceConnectorIds(): SourceConnectorId[] {
  return [...SOURCE_CONNECTORS];
}

export function sourceRequiresApiKey(connector: SourceConnectorId): connector is ApiKeySourceConnectorId {
  return connector === "granola" || connector === "linear";
}

export function sourceSkipsManualMountStep(connector: SourceConnectorId): boolean {
  return connector !== "notion";
}

export function sourceMountRetryOutcome(
  report: { ok: boolean; message: string },
): SourceMountRetryOutcome {
  if (report.ok) {
    return { kind: "success", message: report.message };
  }
  if (classifyMountSetupError(report.message).kind === "file-provider-disabled") {
    return { kind: "retry" };
  }
  return { kind: "error", message: report.message };
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

export function isSourceConnectorId(value: string): value is SourceConnectorId {
  return SOURCE_CONNECTORS.includes(value as SourceConnectorId);
}

export function sourceConnectionReady(
  snapshot: SourceSnapshotLike,
  connector: SourceConnectorId,
): boolean {
  return sourceConnections(snapshot).some(
    (connection) => connection.connector === connector && sourceConnectionStatusReady(connection.status),
  );
}

export function sourceMounted(
  snapshot: SourceSnapshotLike,
  connector: SourceConnectorId,
): boolean {
  return sourceMounts(snapshot).some(
    (mount) => mount.connector === connector && sourceMountStatusMounted(mount.status),
  );
}

export function connectedSourcesReadyToMount(snapshot: SourceSnapshotLike): SourceConnectorId[] {
  return SOURCE_CONNECTORS.filter(
    (connector) => sourceConnectionReady(snapshot, connector) && !sourceMounted(snapshot, connector),
  );
}

function sourceConnections(snapshot: SourceSnapshotLike): SourceConnectionLike[] {
  const byConnector = new Map<string, SourceConnectionLike>();
  for (const connection of snapshot.connections ?? []) {
    if (connection?.connector) {
      byConnector.set(connection.connector, connection);
    }
  }
  if (snapshot.connection?.connector && !byConnector.has(snapshot.connection.connector)) {
    byConnector.set(snapshot.connection.connector, snapshot.connection);
  }
  return Array.from(byConnector.values());
}

function sourceMounts(snapshot: SourceSnapshotLike): SourceMountLike[] {
  const mounts = [...(snapshot.mounts ?? [])];
  if (snapshot.mount?.connector && !mounts.some((mount) => mount.connector === snapshot.mount?.connector)) {
    mounts.push(snapshot.mount);
  }
  return mounts;
}

function sourceConnectionStatusReady(status: string): boolean {
  const normalized = status.trim().toLowerCase();
  return normalized === "active" || normalized === "ready";
}

function sourceMountStatusMounted(status?: string | null): boolean {
  const normalized = status?.trim().toLowerCase() ?? "";
  return normalized !== "not_mounted" && normalized !== "reconnect_needed";
}
