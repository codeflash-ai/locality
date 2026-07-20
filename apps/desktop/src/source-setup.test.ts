import { describe, expect, it } from "vitest";
import {
  connectedSourcesReadyToMount,
  sourceConnectorIds,
  sourceRequiresApiKey,
  sourceSkipsManualMountStep,
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

  it("includes Linear and Slack in the desktop source catalog with their auth modes", () => {
    expect(sourceConnectorIds()).toContain("linear");
    expect(sourceConnectorIds()).toContain("slack");
    expect(sourceRequiresApiKey("linear")).toBe(true);
    expect(sourceRequiresApiKey("granola")).toBe(true);
    expect(sourceRequiresApiKey("slack")).toBe(false);
    expect(sourceRequiresApiKey("gmail")).toBe(false);
    expect(sourceSkipsManualMountStep("linear")).toBe(true);
    expect(sourceSkipsManualMountStep("slack")).toBe(true);
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

  it("includes Slack in ready-to-mount connector ordering", () => {
    expect(
      connectedSourcesReadyToMount({
        connections: [
          { connector: "slack", status: "active" },
          { connector: "gmail", status: "active" },
        ],
        mounts: [{ connector: "gmail", status: "ready" }],
      }),
    ).toEqual(["slack"]);
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
