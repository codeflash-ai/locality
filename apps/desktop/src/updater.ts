import type { DownloadEvent, Update } from "@tauri-apps/plugin-updater";

export type UpdateDownloadProgress = {
  downloadedBytes: number;
  totalBytes?: number;
};

export type UpdateStatus = {
  state:
    | "idle"
    | "checking"
    | "available"
    | "downloading"
    | "downloaded"
    | "installing"
    | "current"
    | "error";
  message: string;
  update: Update | null;
  version?: string;
  progress?: UpdateDownloadProgress;
};

export function emptyUpdateStatus(): UpdateStatus {
  return { state: "idle", message: "", update: null };
}

export function updateErrorMessage(error: unknown) {
  const message = error instanceof Error ? error.message : String(error);
  const lower = message.toLowerCase();
  if (lower.includes("updater") && (lower.includes("config") || lower.includes("endpoint"))) {
    return "Updates are not configured for this build.";
  }
  return message;
}

export function availableUpdateStatus(update: Update): UpdateStatus {
  return {
    state: "available",
    message: `Locality ${update.version} is available.`,
    update,
    version: update.version,
  };
}

export function nextUpdateDownloadProgress(
  current: UpdateDownloadProgress | undefined,
  event: DownloadEvent,
): UpdateDownloadProgress | undefined {
  if (event.event === "Started") {
    return {
      downloadedBytes: 0,
      totalBytes: event.data.contentLength,
    };
  }
  if (event.event === "Progress") {
    return {
      downloadedBytes: (current?.downloadedBytes || 0) + event.data.chunkLength,
      totalBytes: current?.totalBytes,
    };
  }
  if (event.event === "Finished") {
    return current;
  }
  return current;
}

export function updateDownloadMessage(version: string | undefined, progress?: UpdateDownloadProgress) {
  const prefix = version ? `Downloading Locality ${version}` : "Downloading update";
  const percent = updateDownloadPercent(progress);
  return percent === null ? `${prefix}.` : `${prefix} (${percent}%).`;
}

export function updateDownloadPercent(progress?: UpdateDownloadProgress) {
  if (!progress?.totalBytes || progress.totalBytes <= 0) {
    return null;
  }
  return Math.max(0, Math.min(100, Math.round((progress.downloadedBytes / progress.totalBytes) * 100)));
}

export function downloadedUpdateMessage(version: string | undefined) {
  return version
    ? `Locality ${version} is downloaded. Restart to finish installing.`
    : "Update downloaded. Restart to finish installing.";
}

export function installingUpdateMessage(version: string | undefined) {
  return version ? `Installing Locality ${version}.` : "Installing update.";
}

export function updateStatusLabel(status: UpdateStatus, appStoreDistribution: boolean) {
  if (appStoreDistribution) {
    return "Managed by the App Store";
  }
  return status.message || "Ready";
}

export function updateNoticeVisible(status: UpdateStatus) {
  return (
    status.state === "available" ||
    status.state === "downloading" ||
    status.state === "downloaded" ||
    status.state === "installing"
  );
}

export function updateInstallActionLabel(status: UpdateStatus) {
  if (status.state === "downloaded") {
    return "Restart";
  }
  if (status.state === "downloading") {
    return "Downloading";
  }
  if (status.state === "installing") {
    return "Installing";
  }
  return "Install";
}

export function updateInstallActionDisabled(status: UpdateStatus) {
  return status.state === "downloading" || status.state === "installing" || status.state === "checking";
}

export function updateSidebarTitle(status: UpdateStatus) {
  if (status.state === "downloaded") {
    return "Restart to update";
  }
  if (status.state === "downloading") {
    return "Downloading update";
  }
  if (status.state === "installing") {
    return "Installing update";
  }
  return "Update available";
}

export function updateSidebarSubtitle(status: UpdateStatus) {
  const version = status.version ? `v${status.version}` : "Locality";
  const percent = status.state === "downloading" ? updateDownloadPercent(status.progress) : null;
  if (percent !== null) {
    return `${version} · ${percent}%`;
  }
  return version;
}
