import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { signLocalHandoffState, verifyLocalHandoffState } from "../src/security/session";

const secret = "test-session-secret-with-enough-entropy";

describe("local OAuth handoff state", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-07-29T12:00:00Z"));
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("round-trips a Notion local handoff state without storing server state", async () => {
    const token = await signLocalHandoffState(
      {
        v: 1,
        kind: "local_handoff",
        connector: "notion",
        local_redirect_uri: "http://localhost:8757/oauth/notion/callback",
        provider_redirect_uri: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback",
        iat: 1785326400,
        exp: 1785327000,
        nonce: "nonce-1"
      },
      secret
    );

    const payload = await verifyLocalHandoffState(token, secret, 1785326401);

    expect(payload).toEqual({
      v: 1,
      kind: "local_handoff",
      connector: "notion",
      local_redirect_uri: "http://localhost:8757/oauth/notion/callback",
      provider_redirect_uri: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback",
      iat: 1785326400,
      exp: 1785327000,
      nonce: "nonce-1"
    });
  });

  it("rejects a tampered local handoff state", async () => {
    const token = await signLocalHandoffState(
      {
        v: 1,
        kind: "local_handoff",
        connector: "notion",
        local_redirect_uri: "http://localhost:8757/oauth/notion/callback",
        provider_redirect_uri: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback",
        iat: 1785326400,
        exp: 1785327000,
        nonce: "nonce-1"
      },
      secret
    );
    const [body, signature] = token.split(".");
    if (!signature) {
      throw new Error("signed state token is missing a signature");
    }
    const replacement = signature.endsWith("A") ? "B" : "A";
    const tampered = `${body}.${signature.slice(0, -1)}${replacement}`;

    await expect(verifyLocalHandoffState(tampered, secret, 1785326401)).rejects.toMatchObject({
      status: 401,
      code: "invalid_state"
    });
  });

  it("rejects an expired local handoff state", async () => {
    const token = await signLocalHandoffState(
      {
        v: 1,
        kind: "local_handoff",
        connector: "notion",
        local_redirect_uri: "http://localhost:8757/oauth/notion/callback",
        provider_redirect_uri: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback",
        iat: 1785326400,
        exp: 1785327000,
        nonce: "nonce-1"
      },
      secret
    );

    await expect(verifyLocalHandoffState(token, secret, 1785327000)).rejects.toMatchObject({
      status: 401,
      code: "expired_state"
    });
  });
});
