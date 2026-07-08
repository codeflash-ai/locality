import { describe, expect, it } from "vitest";
import {
  downloadedUpdateMessage,
  nextUpdateDownloadProgress,
  updateDownloadMessage,
  updateDownloadPercent,
  updateInstallActionDisabled,
  updateInstallActionLabel,
  updateNoticeVisible,
  updateSidebarSubtitle,
  updateSidebarTitle,
  updateStatusLabel,
  type UpdateStatus,
} from "./updater";

describe("updater status helpers", () => {
  it("tracks download progress from updater events", () => {
    const started = nextUpdateDownloadProgress(undefined, {
      event: "Started",
      data: { contentLength: 200 },
    });
    const progressed = nextUpdateDownloadProgress(started, {
      event: "Progress",
      data: { chunkLength: 50 },
    });

    expect(progressed).toEqual({ downloadedBytes: 50, totalBytes: 200 });
    expect(updateDownloadPercent(progressed)).toBe(25);
    expect(updateDownloadMessage("0.1.6", progressed)).toBe("Downloading Locality 0.1.6 (25%).");
  });

  it("keeps indeterminate download progress readable", () => {
    const progress = nextUpdateDownloadProgress(undefined, {
      event: "Started",
      data: {},
    });

    expect(updateDownloadPercent(progress)).toBeNull();
    expect(updateDownloadMessage("0.1.6", progress)).toBe("Downloading Locality 0.1.6.");
  });

  it("shows restart as the install action after download", () => {
    const status: UpdateStatus = {
      state: "downloaded",
      message: downloadedUpdateMessage("0.1.6"),
      update: null,
      version: "0.1.6",
    };

    expect(updateNoticeVisible(status)).toBe(true);
    expect(updateInstallActionLabel(status)).toBe("Restart");
    expect(updateInstallActionDisabled(status)).toBe(false);
    expect(updateSidebarTitle(status)).toBe("Restart to update");
    expect(updateSidebarSubtitle(status)).toBe("v0.1.6");
    expect(updateStatusLabel(status, false)).toBe("Locality 0.1.6 is downloaded. Restart to finish installing.");
  });

  it("disables install actions while a download is in progress", () => {
    const status: UpdateStatus = {
      state: "downloading",
      message: "Downloading Locality 0.1.6.",
      update: null,
      version: "0.1.6",
    };

    expect(updateNoticeVisible(status)).toBe(true);
    expect(updateInstallActionLabel(status)).toBe("Downloading");
    expect(updateInstallActionDisabled(status)).toBe(true);
  });

  it("shows sidebar download progress when the updater reports a content length", () => {
    const status: UpdateStatus = {
      state: "downloading",
      message: "Downloading Locality 0.1.6 (25%).",
      update: null,
      version: "0.1.6",
      progress: { downloadedBytes: 50, totalBytes: 200 },
    };

    expect(updateSidebarTitle(status)).toBe("Downloading update");
    expect(updateSidebarSubtitle(status)).toBe("v0.1.6 · 25%");
  });
});
