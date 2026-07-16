export type ProviderRuntimeSummary = {
  state: string;
  message: string;
  daemonRunning: boolean;
  registered?: boolean | null;
  pid?: number | null;
  stalePidFile: boolean;
};

export type MountHydrationProgress = {
  indexedFiles: number;
  remainingFiles: number;
  totalFiles: number;
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
  hydrationProgress?: MountHydrationProgress | null;
  pendingChangeCount: number;
  provider?: ProviderRuntimeSummary | null;
};

export type MountRow = {
  id: string;
  mount: MountSummary;
  title: string;
  subtitle: string;
  localPath: string;
  displayPath: string;
  projection: string;
  access: string;
  content: string;
  status: string;
  tone: "ready" | "warn" | "danger";
  active: boolean;
};

export type SourceDestructiveAction = "reset" | "disconnect";

export function sourceDestructiveConfirmation(
  action: SourceDestructiveAction,
  mountId: string,
): string {
  const verb = action === "reset" ? "RESET" : "DISCONNECT";
  return `${verb} ${mountId.trim()}`;
}

export function sourceDestructiveConfirmationMatches(
  action: SourceDestructiveAction,
  mountId: string,
  value: string,
): boolean {
  return value.trim() === sourceDestructiveConfirmation(action, mountId);
}

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
    displayPath: compactPath(mount.localPath),
    projection: mount.projection,
    access: mountAccessLabel(mount),
    content: mountContentLabel(mount),
    status: mountStatusLabel(mount),
    tone: mountStatusTone(mount),
    active: activeMountId === mount.mountId,
  }));
}

export function selectedMountRow(
  rows: MountRow[],
  selectedMountId: string | null | undefined,
): MountRow | null {
  if (!selectedMountId) {
    return null;
  }
  return rows.find((row) => row.id === selectedMountId) ?? null;
}

export function selectedMountIdAfterViewChange(
  selectedMountId: string | null,
  nextView: string,
): string | null {
  return nextView === "mount" ? selectedMountId : null;
}

export function selectedMountIdAfterOpenViewEvent(
  selectedMountId: string | null,
  nextView: string,
): string | null {
  return nextView === "mount" ? null : selectedMountId;
}

export function mountAccessLabel(mount: MountSummary): string {
  return mount.readOnly ? "Read only" : "Edit enabled";
}

export function mountStatusLabel(mount: MountSummary): string {
  if (!isReadyStatus(mount.status)) {
    return titleFromStatus(mount.status);
  }
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
    mount.provider?.registered === false ||
    providerState.includes("unregistered") ||
    providerState.includes("stale") ||
    status.includes("unregistered") ||
    status.includes("preparing") ||
    status.includes("pending") ||
    status.includes("review") ||
    status.includes("not_mounted")
  ) {
    return "warn";
  }
  return "ready";
}

export function mountEntityCountLabel(mount: MountSummary): string {
  return `${mount.entityCount} ${mount.entityCount === 1 ? "item" : "items"}`;
}

export function mountFileIndexProgressLabel(mount: MountSummary): string | null {
  const value = mountFileIndexProgressValue(mount);
  return value ? `Indexed: ${value}` : null;
}

export function mountFileIndexProgressValue(mount: MountSummary): string | null {
  const progress = mount.hydrationProgress;
  if (!progress || progress.totalFiles <= 0) {
    return null;
  }

  const fileLabel = progress.totalFiles === 1 ? "file" : "files";
  const base = `${progress.indexedFiles} of ${progress.totalFiles} ${fileLabel}`;
  if (progress.remainingFiles <= 0) {
    return base;
  }
  return `${base}, ${progress.remainingFiles} left`;
}

export function compactPath(path: string, maxLength = 64): string {
  const trimmed = path.trim();
  if (trimmed.length <= maxLength) {
    return trimmed;
  }

  const separator = trimmed.includes("\\") && !trimmed.includes("/") ? "\\" : "/";
  const normalized = trimmed.replace(/[\\/]+/g, separator).replace(/[\\/]+$/, "");
  const parts = normalized.split(/[\\/]+/).filter(Boolean);
  if (parts.length <= 1) {
    return truncateLeading(trimmed, maxLength);
  }

  const prefix = pathPrefix(normalized, parts[0] ?? "", separator);
  let best = "";
  for (let tailCount = 1; tailCount <= parts.length; tailCount += 1) {
    const tail = parts.slice(-tailCount).join(separator);
    const candidate = compactPathCandidate(prefix, separator, tail);
    if (candidate.length <= maxLength) {
      best = candidate;
    } else if (best) {
      break;
    }
  }

  return best || truncateLeading(trimmed, maxLength);
}

function isReadyStatus(status: string): boolean {
  return status.trim().toLowerCase() === "ready";
}

function pathPrefix(path: string, firstPart: string, separator: string): string {
  if (path.startsWith(`~${separator}`)) {
    return "~";
  }
  if (path.startsWith(separator)) {
    return "";
  }
  if (/^[A-Za-z]:$/.test(firstPart)) {
    return firstPart;
  }
  return firstPart;
}

function compactPathCandidate(prefix: string, separator: string, tail: string): string {
  if (!prefix && separator === "/") {
    return `/.../${tail}`;
  }
  if (prefix === "~") {
    return `~/.../${tail}`;
  }
  if (/^[A-Za-z]:$/.test(prefix)) {
    return `${prefix}${separator}...${separator}${tail}`;
  }
  return `${prefix}${separator}...${separator}${tail}`;
}

function truncateLeading(value: string, maxLength: number): string {
  if (maxLength <= 3) {
    return value.slice(0, Math.max(0, maxLength));
  }
  return `...${value.slice(-(maxLength - 3))}`;
}

function isRealMount(mount: MountSummary): boolean {
  return mount.mountId.trim().length > 0 && mount.status !== "not_mounted";
}

function mountSubtitle(mount: MountSummary): string {
  const workspace = mount.workspaceName.trim();
  const displayName = mountPathName(mount.localPath) || mount.mountId;
  if (workspace.length > 0) {
    return `${workspace} / ${displayName}`;
  }
  return displayName;
}

function mountPathName(path: string): string {
  return path.trim().replace(/[\\/]+$/, "").split(/[\\/]/).filter(Boolean).pop() ?? "";
}

function mountContentLabel(mount: MountSummary): string {
  const fileProgress = mountFileIndexProgressLabel(mount);
  if (mount.hydrationProgress && mount.hydrationProgress.remainingFiles > 0 && fileProgress) {
    return fileProgress;
  }

  const itemLabel = mountEntityCountLabel(mount);
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
