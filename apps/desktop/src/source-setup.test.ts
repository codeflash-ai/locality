import { describe, expect, it } from "vitest";
import {
  sourceConnectorIds,
  sourceRequiresApiKey,
  sourceSkipsManualMountStep,
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

  it("includes Linear in the desktop source catalog as an API-key connector", () => {
    expect(sourceConnectorIds()).toContain("linear");
    expect(sourceRequiresApiKey("linear")).toBe(true);
    expect(sourceRequiresApiKey("granola")).toBe(true);
    expect(sourceRequiresApiKey("gmail")).toBe(false);
    expect(sourceSkipsManualMountStep("linear")).toBe(true);
  });
});
