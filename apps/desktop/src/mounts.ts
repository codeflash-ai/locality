export type ProviderRuntimeSummary = {
  state: string;
  message: string;
  daemonRunning: boolean;
  registered?: boolean | null;
  pid?: number | null;
  stalePidFile: boolean;
};

export type MountSummary = {
  mountId: string;
  connector: string;
  connectorName: string;
  connectionId?: string | null;
  workspaceName: string;
  localPath: string;
  notionUrl?: string | null;
  accessScope: string;
  remoteRootId?: string | null;
  projection: string;
  readOnly: boolean;
  status: string;
  rootExists: boolean;
  entityCount: number;
  pendingChangeCount: number;
  provider?: ProviderRuntimeSummary | null;
};

export type MountRow = {
  id: string;
  mount: MountSummary;
  title: string;
  subtitle: string;
  localPath: string;
  projection: string;
  access: string;
  content: string;
  status: string;
  tone: "ready" | "warn" | "danger";
  active: boolean;
};

export function mountRows(
  mounts: MountSummary[],
  fallbackMount: MountSummary,
  activeMountId?: string | null,
): MountRow[] {
  const source = mounts.length > 0 ? mounts : [fallbackMount];

  return source.filter(isRealMount).map((mount) => ({
    id: mount.mountId,
    mount,
    title: mount.connectorName || mount.connector,
    subtitle: mountSubtitle(mount),
    localPath: mount.localPath,
    projection: mount.projection,
    access: mountAccessLabel(mount),
    content: mountContentLabel(mount),
    status: mountStatusLabel(mount),
    tone: mountStatusTone(mount),
    active: activeMountId === mount.mountId,
  }));
}

export function selectedMountRow(rows: MountRow[], selectedMountId: string | null): MountRow | null {
  if (!selectedMountId) {
    return null;
  }
  return rows.find((row) => row.id === selectedMountId) ?? null;
}

export function mountAccessLabel(mount: MountSummary): string {
  return mount.readOnly ? "Read only" : "Edit enabled";
}

export function mountStatusLabel(mount: MountSummary): string {
  const providerMessage = mount.provider?.message?.trim();
  if (providerMessage) {
    return providerMessage;
  }
  return titleFromStatus(mount.status);
}

export function mountStatusTone(mount: MountSummary): "ready" | "warn" | "danger" {
  const providerState = mount.provider?.state?.toLowerCase() ?? "";
  const status = mount.status.toLowerCase();
  if (
    providerState.includes("error") ||
    status.includes("error") ||
    status.includes("stopped") ||
    status.includes("conflict") ||
    status.includes("missing")
  ) {
    return "danger";
  }
  if (
    providerState.includes("stale") ||
    status.includes("preparing") ||
    status.includes("pending") ||
    status.includes("review") ||
    status.includes("not_mounted")
  ) {
    return "warn";
  }
  return "ready";
}

function isRealMount(mount: MountSummary): boolean {
  return mount.mountId.trim().length > 0 && mount.status !== "not_mounted";
}

function mountSubtitle(mount: MountSummary): string {
  const workspace = mount.workspaceName.trim();
  if (workspace.length > 0) {
    return `${workspace} / ${mount.mountId}`;
  }
  return mount.mountId;
}

function mountContentLabel(mount: MountSummary): string {
  const itemLabel = `${mount.entityCount} ${mount.entityCount === 1 ? "item" : "items"}`;
  if (mount.pendingChangeCount === 0) {
    return itemLabel;
  }
  return `${itemLabel}, ${mount.pendingChangeCount} pending`;
}

function titleFromStatus(status: string): string {
  const words = status
    .replace(/[_-]+/g, " ")
    .trim()
    .split(/\s+/)
    .filter(Boolean);
  if (words.length === 0) {
    return "Unknown";
  }
  return words
    .map((word, index) => {
      const lower = word.toLowerCase();
      return index === 0 ? lower.charAt(0).toUpperCase() + lower.slice(1) : lower;
    })
    .join(" ");
}
