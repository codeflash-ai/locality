import { describe, expect, it } from "vitest";
import {
  mountAccessLabel,
  mountRows,
  mountStatusLabel,
  mountStatusTone,
  selectedMountIdAfterOpenViewEvent,
  selectedMountIdAfterViewChange,
  selectedMountRow,
  type MountSummary,
} from "./mounts";

function mount(overrides: Partial<MountSummary>): MountSummary {
  return {
    mountId: "notion-main",
    connector: "notion",
    connectorName: "Notion",
    connectionId: "notion-default",
    workspaceName: "CodeFlash",
    localPath: "/home/ada/Locality/notion",
    notionUrl: "https://www.notion.so/example",
    accessScope: "Workspace",
    remoteRootId: "notion-root",
    projection: "Linux FUSE",
    readOnly: false,
    status: "ready",
    rootExists: true,
    entityCount: 24,
    pendingChangeCount: 3,
    provider: null,
    ...overrides,
  };
}

describe("mount display helpers", () => {
  it("returns no rows for the empty not-mounted fallback summary", () => {
    const fallback = mount({
      mountId: "",
      workspaceName: "No mounted workspace",
      localPath: "/home/ada/Locality/notion",
      status: "not_mounted",
      entityCount: 0,
      pendingChangeCount: 0,
    });

    expect(mountRows([], fallback, null)).toEqual([]);
  });

  it("builds one row per real mount while preserving snapshot order", () => {
    const notion = mount({});
    const google = mount({
      mountId: "google-docs-main",
      connector: "google-docs",
      connectorName: "Google Docs",
      connectionId: "google-docs-default",
      workspaceName: "Drive",
      localPath: "/home/ada/Locality/google-docs-main",
      notionUrl: null,
      accessScope: "Workspace folder",
      remoteRootId: "drive-folder-1",
      readOnly: true,
      entityCount: 4,
      pendingChangeCount: 0,
    });

    const rows = mountRows([notion, google], notion, "notion-main");

    expect(rows.map((row) => row.id)).toEqual(["notion-main", "google-docs-main"]);
    expect(rows[0]).toMatchObject({
      title: "Notion",
      subtitle: "CodeFlash / notion",
      localPath: "/home/ada/Locality/notion",
      projection: "Linux FUSE",
      access: "Edit enabled",
      content: "24 items, 3 pending",
      status: "Ready",
      tone: "ready",
      active: true,
    });
    expect(rows[1]).toMatchObject({
      title: "Google Docs",
      subtitle: "Drive / google-docs-main",
      access: "Read only",
      content: "4 items",
      active: false,
    });
  });

  it("selects the clicked mount row by mount id", () => {
    const notion = mount({});
    const google = mount({
      mountId: "google-docs-main",
      connector: "google-docs",
      connectorName: "Google Docs",
      workspaceName: "Drive",
      localPath: "/home/ada/Locality/google-docs-main",
    });
    const rows = mountRows([notion, google], notion, "notion-main");

    expect(selectedMountRow(rows, "google-docs-main")?.mount).toEqual(google);
    expect(selectedMountRow(rows, "missing")).toBeNull();
    expect(selectedMountRow(rows, null)).toBeNull();
    expect(selectedMountRow(rows, undefined)).toBeNull();
  });

  it("uses mount and provider state for readable labels", () => {
    expect(mountAccessLabel(mount({ readOnly: false }))).toBe("Edit enabled");
    expect(mountAccessLabel(mount({ readOnly: true }))).toBe("Read only");
    expect(mountStatusLabel(mount({ status: "runtime_stopped" }))).toBe("Runtime stopped");
    expect(
      mountStatusLabel(
        mount({
          provider: {
            state: "registered",
            message: "registered=true active=false",
            daemonRunning: false,
            registered: true,
            pid: null,
            stalePidFile: false,
          },
        }),
      ),
    ).toBe("registered=true active=false");
  });

  it("uses non-ready mount statuses instead of generic provider messages", () => {
    expect(
      mountStatusLabel(
        mount({
          status: "provider_unregistered",
          provider: {
            state: "running",
            message: "running",
            daemonRunning: true,
            registered: false,
            pid: 123,
            stalePidFile: false,
          },
        }),
      ),
    ).toBe("Provider unregistered");
  });

  it("classifies provider mount statuses by readiness", () => {
    expect(
      mountStatusTone(
        mount({
          status: "provider_unregistered",
          provider: {
            state: "running",
            message: "running",
            daemonRunning: true,
            registered: false,
            pid: 123,
            stalePidFile: false,
          },
        }),
      ),
    ).toBe("warn");
    expect(
      mountStatusTone(
        mount({
          status: "ready",
          provider: {
            state: "running",
            message: "running",
            daemonRunning: true,
            registered: false,
            pid: 123,
            stalePidFile: false,
          },
        }),
      ),
    ).toBe("warn");
    expect(mountStatusTone(mount({ status: "provider_stopped" }))).toBe("danger");
    expect(mountStatusTone(mount({ status: "provider_error" }))).toBe("danger");
  });
});

describe("selected mount navigation helpers", () => {
  it("clears the selected mount when leaving the mount view", () => {
    expect(selectedMountIdAfterViewChange("google-docs-main", "home")).toBeNull();
  });

  it("keeps the selected mount while staying on the mount view", () => {
    expect(selectedMountIdAfterViewChange("google-docs-main", "mount")).toBe("google-docs-main");
    expect(selectedMountIdAfterViewChange(null, "mount")).toBeNull();
  });

  it("clears the selected mount when an open-view event requests the mount list", () => {
    expect(selectedMountIdAfterOpenViewEvent("google-docs-main", "mount")).toBeNull();
  });

  it("leaves the selected mount unchanged when an open-view event requests another view", () => {
    expect(selectedMountIdAfterOpenViewEvent("google-docs-main", "pending")).toBe("google-docs-main");
    expect(selectedMountIdAfterOpenViewEvent(null, "pending")).toBeNull();
  });
});
