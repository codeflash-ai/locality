import { describe, expect, it } from "vitest";
import {
  connectedSourcesReadyToMount,
  isSourceCatalogConnectorId,
  isSourceConnectorId,
  plannedSourceConnectorDefinitions,
  plannedSourceConnectorIds,
  sourceCatalogConnectorDefinition,
  sourceConnectorCatalogDefinitions,
  sourceConnectorIds,
  sourceConnectorDefaultMountDirectory,
  sourceConnectorDefaultMountId,
  sourceConnectorDefinition,
  sourceConnectorDefinitions,
  sourceConnectorName,
  sourceRequiresApiKey,
  sourceSkipsManualMountStep,
  sourceMounted,
  sourceMountRetryOutcome,
  sourceSetupIsActiveConnector,
  sourceSetupIsBusy,
  sourceSetupProgressLabel,
} from "./source-setup";

describe("source setup progress", () => {
  it("marks only the connector running the shared setup operation as active", () => {
    expect(sourceSetupIsBusy("connecting")).toBe(true);
    expect(sourceSetupIsActiveConnector("connecting", "granola", "granola")).toBe(true);
    expect(sourceSetupIsActiveConnector("connecting", "granola", "notion")).toBe(false);
    expect(sourceSetupIsActiveConnector("success", "granola", "granola")).toBe(false);
  });

  it("distinguishes connection, mounting, provider finishing, and access changes", () => {
    expect(sourceSetupProgressLabel("connecting", false)).toBe("Connecting");
    expect(sourceSetupProgressLabel("creating", false)).toBe("Mounting");
    expect(sourceSetupProgressLabel("connecting", true)).toBe("Finishing setup");
    expect(sourceSetupProgressLabel("changing", true)).toBe("Updating access");
  });

  it("includes connector catalog entries with their auth modes", () => {
    expect(sourceConnectorIds()).toEqual([
      "notion",
      "google-docs",
      "google-calendar",
      "gmail",
      "granola",
      "linear",
      "slack",
    ]);
    expect(sourceConnectorDefinitions().map((connector) => connector.id)).toEqual(sourceConnectorIds());
    expect(sourceRequiresApiKey("linear")).toBe(true);
    expect(sourceRequiresApiKey("granola")).toBe(true);
    expect(sourceRequiresApiKey("slack")).toBe(false);
    expect(sourceRequiresApiKey("google-calendar")).toBe(false);
    expect(sourceRequiresApiKey("gmail")).toBe(false);
    expect(sourceSkipsManualMountStep("linear")).toBe(true);
    expect(sourceSkipsManualMountStep("slack")).toBe(true);
    expect(sourceSkipsManualMountStep("google-calendar")).toBe(true);
    expect(sourceConnectorName("google-docs")).toBe("Google Docs");
    expect(sourceConnectorDefaultMountId("google-docs")).toBe("google-docs-main");
    expect(sourceConnectorDefaultMountDirectory("google-docs")).toBe("google-docs-main");
    expect(sourceConnectorDefinition("notion").availability).toBe("implemented");
    expect(sourceConnectorDefinition("notion").projection).toContain("page.md");
    expect(sourceConnectorDefinition("slack").writeModel).toBe("Read-only.");
  });

  it("keeps planned connector catalog entries separate from runtime setup", () => {
    expect(plannedSourceConnectorIds()).toEqual([
      "confluence",
      "jira",
      "sharepoint",
      "onedrive",
      "outlook-mail",
      "outlook-calendar",
      "microsoft-teams",
      "github",
      "gitlab",
      "google-drive",
      "dropbox",
      "box",
      "figma",
      "asana",
      "clickup",
      "zendesk",
      "intercom",
      "hubspot",
      "salesforce",
      "fhir",
    ]);
    expect(sourceConnectorCatalogDefinitions()).toHaveLength(27);
    expect(plannedSourceConnectorDefinitions()).toHaveLength(20);
    expect(isSourceConnectorId("confluence")).toBe(false);
    expect(isSourceCatalogConnectorId("confluence")).toBe(true);
    expect(isSourceCatalogConnectorId("notion")).toBe(true);
    expect(sourceCatalogConnectorDefinition("github").authModes).toEqual([
      "oauth",
      "github-app",
      "personal-token",
    ]);
    expect(sourceCatalogConnectorDefinition("fhir").authModes).toEqual(["smart-oauth"]);
    expect(sourceCatalogConnectorDefinition("confluence").availability).toBe("planned");
    expect(sourceCatalogConnectorDefinition("confluence").projection).toContain("Spaces");
    expect(sourceCatalogConnectorDefinition("github").writeModel).toContain("repository edits stay in git");
    expect(sourceCatalogConnectorDefinition("fhir").writeModel).toContain("Read-only");
  });

  it("keeps connected but unmounted sources visible when another source is mounted", () => {
    expect(
      connectedSourcesReadyToMount({
        connection: { connector: "granola", status: "active" },
        connections: [
          { connector: "granola", status: "active" },
          { connector: "notion", status: "active" },
        ],
        mounts: [{ connector: "granola", status: "ready" }],
      }),
    ).toEqual(["notion"]);
  });

  it("falls back to the selected connection when older snapshots do not include all connections", () => {
    expect(
      connectedSourcesReadyToMount({
        connection: { connector: "notion", status: "active" },
        mounts: [],
      }),
    ).toEqual(["notion"]);
  });

  it("includes connected unmounted sources in catalog order", () => {
    expect(isSourceConnectorId("google-calendar")).toBe(true);
    expect(
      connectedSourcesReadyToMount({
        connections: [
          { connector: "google-calendar", status: "active" },
          { connector: "slack", status: "active" },
          { connector: "gmail", status: "active" },
        ],
        mounts: [{ connector: "gmail", status: "ready" }],
      }),
    ).toEqual(["google-calendar", "slack"]);
  });

  it("does not treat a retained disconnected source mount as mounted", () => {
    expect(
      sourceMounted(
        {
          mounts: [{ connector: "notion", status: "reconnect_needed" }],
        },
        "notion",
      ),
    ).toBe(false);
  });
});

describe("source File Provider mount retry", () => {
  it("completes a successful automatic mount retry", () => {
    expect(sourceMountRetryOutcome({ ok: true, message: "Mounted Notion." })).toEqual({
      kind: "success",
      message: "Mounted Notion.",
    });
  });

  it("continues recovery when File Provider is still disabled", () => {
    expect(sourceMountRetryOutcome({
      ok: false,
      message: "The Locality File Provider is registered but not enabled.",
    })).toEqual({ kind: "retry" });
  });

  it("turns another automatic mount failure into a visible dialog error", () => {
    expect(sourceMountRetryOutcome({
      ok: false,
      message: "Could not load the top-level Notion folder.",
    })).toEqual({
      kind: "error",
      message: "Could not load the top-level Notion folder.",
    });
  });
});
