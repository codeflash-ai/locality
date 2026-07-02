import { describe, expect, it } from "vitest";
import { connectionMissing, connectionReady } from "./connection-state";

function snapshot(status: string) {
  return {
    connection: {
      status,
    },
  };
}

describe("connection state helpers", () => {
  it("treats only active connections as ready", () => {
    expect(connectionReady(snapshot("active"))).toBe(true);
    expect(connectionReady(snapshot("missing"))).toBe(false);
    expect(connectionReady(snapshot("revoked"))).toBe(false);
    expect(connectionReady(snapshot("error"))).toBe(false);
  });

  it("treats inactive and failed connections as missing for UI gating", () => {
    expect(connectionMissing(snapshot("missing"))).toBe(true);
    expect(connectionMissing(snapshot("revoked"))).toBe(true);
    expect(connectionMissing(snapshot("error"))).toBe(true);
    expect(connectionMissing(snapshot("active"))).toBe(false);
  });
});
