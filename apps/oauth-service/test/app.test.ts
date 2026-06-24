import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import app from "../src/app";
import { hmacSha256Base64Url, utf8Base64Url } from "../src/security/crypto";
import type { BrokerEnv } from "../src/types";

interface StartResponse {
  connector: string;
  client_id: string;
  authorization_url: string;
  redirect_uri: string;
  session: string;
  state: string;
}

interface BrokerTokenResponse {
  access_token: string;
  refresh_token?: string;
  refresh_token_kind?: string;
  refresh_token_handle?: string;
}

const env: BrokerEnv = {
  LOCALITY_BROKER_SESSION_SECRET: "test-session-secret-with-enough-entropy",
  LOCALITY_REFRESH_HANDLE_KEY: "test-refresh-handle-key-with-enough-entropy",
  LOCALITY_TOKEN_MODE: "handle",
  LOCALITY_NOTION_CLIENT_ID: "notion-client-id",
  LOCALITY_NOTION_CLIENT_SECRET: "notion-client-secret",
  LOCALITY_NOTION_API_BASE_URL: "https://notion.example.test",
  LOCALITY_NOTION_AUTH_BASE_URL: "https://notion.example.test",
  LOCALITY_NOTION_REDIRECT_URIS: "http://localhost:8757/oauth/notion/callback"
};

describe("auth broker", () => {
  const originalFetch = globalThis.fetch;

  beforeEach(() => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-06-11T12:00:00Z"));
  });

  afterEach(() => {
    globalThis.fetch = originalFetch;
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it("creates a Notion OAuth session and authorization URL", async () => {
    const response = await app.request("/v1/oauth/notion/start", { method: "POST" }, env);
    expect(response.status).toBe(200);
    const body = (await response.json()) as StartResponse;
    expect(body.connector).toBe("notion");
    expect(body.client_id).toBe("notion-client-id");
    expect(body.authorization_url).toContain("client_id=notion-client-id");
    expect(body.authorization_url).toContain("response_type=code");
    expect(body.authorization_url).toContain("owner=user");
    expect(body.redirect_uri).toBe("http://localhost:8757/oauth/notion/callback");
    expect(body.session).toBeTruthy();
    expect(body.state).toBeTruthy();
  });

  it("rejects unconfigured redirect URIs", async () => {
    const response = await app.request(
      "/v1/oauth/notion/start",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ redirect_uri: "http://localhost:9999/oauth/notion/callback" })
      },
      env
    );
    expect(response.status).toBe(400);
    await expect(response.json()).resolves.toMatchObject({
      error: { code: "redirect_uri_not_allowed" }
    });
  });

  it("exchanges an authorization code without exposing the client secret to the caller", async () => {
    const start = await startSession();
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) =>
      Response.json({
        access_token: "access-token",
        refresh_token: "refresh-token",
        token_type: "bearer",
        expires_in: 3600,
        workspace_id: "workspace-id"
      })
    );
    globalThis.fetch = fetchMock as unknown as typeof fetch;

    const response = await app.request(
      "/v1/oauth/notion/exchange",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          session: start.session,
          state: start.state,
          code: "authorization-code",
          redirect_uri: "http://localhost:8757/oauth/notion/callback"
        })
      },
      env
    );

    expect(response.status).toBe(200);
    const body = (await response.json()) as BrokerTokenResponse;
    expect(body.access_token).toBe("access-token");
    expect(body.refresh_token).toBeUndefined();
    expect(body.refresh_token_kind).toBe("handle");
    expect(body.refresh_token_handle).toMatch(/^locrh_v1\./);
    expect(fetchMock).toHaveBeenCalledWith(
      "https://notion.example.test/v1/oauth/token",
      expect.objectContaining({
        method: "POST",
        headers: expect.objectContaining({
          Authorization: `Basic ${btoa("notion-client-id:notion-client-secret")}`
        })
      })
    );
  });

  it("refreshes through an opaque refresh handle", async () => {
    const start = await startSession();
    let calls = 0;
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) => {
      calls += 1;
      if (calls === 1) {
        return Response.json({
          access_token: "access-token",
          refresh_token: "refresh-token",
          expires_in: 3600
        });
      }
      return Response.json({
        access_token: "new-access-token",
        refresh_token: "new-refresh-token",
        expires_in: 3600
      });
    });
    globalThis.fetch = fetchMock as unknown as typeof fetch;

    const exchanged = await app.request(
      "/v1/oauth/notion/exchange",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          session: start.session,
          state: start.state,
          code: "authorization-code",
          redirect_uri: "http://localhost:8757/oauth/notion/callback"
        })
      },
      env
    );
    const exchangeBody = (await exchanged.json()) as BrokerTokenResponse;

    const refreshed = await app.request(
      "/v1/oauth/notion/refresh",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ refresh_token_handle: exchangeBody.refresh_token_handle })
      },
      env
    );

    expect(refreshed.status).toBe(200);
    const refreshBody = (await refreshed.json()) as BrokerTokenResponse;
    expect(refreshBody.access_token).toBe("new-access-token");
    expect(refreshBody.refresh_token_handle).toMatch(/^locrh_v1\./);
    const refreshCall = fetchMock.mock.calls[1];
    expect(refreshCall).toBeDefined();
    const refreshRequest = JSON.parse((refreshCall?.[1] as RequestInit).body as string);
    expect(refreshRequest).toMatchObject({
      grant_type: "refresh_token",
      refresh_token: "refresh-token"
    });
  });

  it("rejects raw refresh tokens when handle mode is enabled", async () => {
    const response = await app.request(
      "/v1/oauth/notion/refresh",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ refresh_token: "raw-refresh-token" })
      },
      env
    );

    expect(response.status).toBe(400);
    await expect(response.json()).resolves.toMatchObject({
      error: { code: "missing_refresh_handle" }
    });
  });

  it("rejects malformed signed session payloads without a 500", async () => {
    const body = utf8Base64Url("not-json");
    const signature = await hmacSha256Base64Url(env.LOCALITY_BROKER_SESSION_SECRET, body);
    const response = await app.request(
      "/v1/oauth/notion/exchange",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          session: `${body}.${signature}`,
          state: "state",
          code: "authorization-code",
          redirect_uri: "http://localhost:8757/oauth/notion/callback"
        })
      },
      env
    );

    expect(response.status).toBe(400);
    await expect(response.json()).resolves.toMatchObject({
      error: { code: "invalid_session" }
    });
  });
});

async function startSession() {
  const response = await app.request("/v1/oauth/notion/start", { method: "POST" }, env);
  expect(response.status).toBe(200);
  return response.json() as Promise<StartResponse>;
}
