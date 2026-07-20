import { describe, expect, it } from "vitest";
import {
  compactPath,
  mountEntityCountLabel,
  mountFileIndexProgressLabel,
  mountAccessLabel,
  mountRows,
  mountStatusLabel,
  mountStatusTone,
  selectedMountIdAfterOpenViewEvent,
  selectedMountIdAfterViewChange,
  selectedMountRow,
  sourceDestructiveConfirmation,
  sourceDestructiveConfirmationMatches,
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
      displayPath: "/home/ada/Locality/notion",
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

  it("builds Google Calendar mount rows with calendar workspace context", () => {
    const notion = mount({});
    const calendar = mount({
      mountId: "google-calendar-main",
      connector: "google-calendar",
      connectorName: "Google Calendar",
      connectionId: "google-calendar-default",
      workspaceName: "Primary calendar",
      localPath: "/home/ada/Locality/google-calendar-main",
      notionUrl: null,
      accessScope: "Primary calendar",
      remoteRootId: null,
      entityCount: 8,
      pendingChangeCount: 0,
    });

    const rows = mountRows([notion, calendar], notion, "google-calendar-main");
    const selected = selectedMountRow(rows, "google-calendar-main");

    expect(selected).toMatchObject({
      title: "Google Calendar",
      subtitle: "Primary calendar / google-calendar-main",
    });
  });

  it("labels indexed mount entities as items, not physical files", () => {
    expect(mountEntityCountLabel(mount({ entityCount: 1 }))).toBe("1 item");
    expect(mountEntityCountLabel(mount({ entityCount: 260 }))).toBe("260 items");
  });

  it("labels hydratable file indexing progress", () => {
    expect(
      mountFileIndexProgressLabel(
        mount({
          hydrationProgress: {
            indexedFiles: 1,
            remainingFiles: 2,
            totalFiles: 3,
          },
        }),
      ),
    ).toBe("Indexed: 1 of 3 files, 2 left");
    expect(
      mountFileIndexProgressLabel(
        mount({
          hydrationProgress: {
            indexedFiles: 1,
            remainingFiles: 0,
            totalFiles: 1,
          },
        }),
      ),
    ).toBe("Indexed: 1 of 1 file");
    expect(mountFileIndexProgressLabel(mount({}))).toBeNull();
  });

  it("uses hydratable file progress as source card content while files remain", () => {
    const rows = mountRows(
      [
        mount({
          hydrationProgress: {
            indexedFiles: 12,
            remainingFiles: 68,
            totalFiles: 80,
          },
        }),
      ],
      mount({}),
      "notion-main",
    );

    expect(rows[0].content).toBe("Indexed: 12 of 80 files, 68 left");
  });

  it("keeps compact source card content once file indexing is complete", () => {
    const rows = mountRows(
      [
        mount({
          hydrationProgress: {
            indexedFiles: 80,
            remainingFiles: 0,
            totalFiles: 80,
          },
        }),
      ],
      mount({}),
      "notion-main",
    );

    expect(rows[0].content).toBe("24 items, 3 pending");
  });

  it("compacts long paths from the middle so filenames remain visible", () => {
    expect(compactPath("/home/ada/Locality/notion/Engineering/Roadmap 2026/page.md", 42)).toBe(
      "/.../Engineering/Roadmap 2026/page.md",
    );
    expect(compactPath("~/Library/CloudStorage/Locality/notion", 64)).toBe(
      "~/Library/CloudStorage/Locality/notion",
    );
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

describe("source destructive action confirmation", () => {
  it("scopes reset and disconnect phrases to the selected mount", () => {
    expect(sourceDestructiveConfirmation("reset", "notion-main")).toBe("RESET notion-main");
    expect(sourceDestructiveConfirmation("disconnect", "granola-main")).toBe(
      "DISCONNECT granola-main",
    );
  });

  it("requires the complete case-sensitive phrase", () => {
    expect(sourceDestructiveConfirmationMatches("reset", "granola-main", " RESET granola-main ")).toBe(true);
    expect(sourceDestructiveConfirmationMatches("reset", "granola-main", "RESET")).toBe(false);
    expect(sourceDestructiveConfirmationMatches("reset", "granola-main", "reset granola-main")).toBe(false);
  });
});
