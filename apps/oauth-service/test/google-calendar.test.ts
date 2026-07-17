import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import app from "../src/app";
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
  scope?: string;
  id_token?: string;
  workspace_id?: string;
  workspace_name?: string;
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
  LOCALITY_GOOGLE_CLIENT_ID: "google-client-id",
  LOCALITY_GOOGLE_CLIENT_SECRET: "google-client-secret",
  LOCALITY_GOOGLE_CALENDAR_API_BASE_URL: "https://oauth2.example.test",
  LOCALITY_GOOGLE_CALENDAR_AUTH_BASE_URL: "https://accounts.example.test",
  LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS: "http://localhost:8757/oauth/google-calendar/callback"
};

describe("Google Calendar OAuth broker", () => {
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

  it("publishes Google Calendar in broker discovery", async () => {
    const response = await app.request("/.well-known/loc-auth-broker", { method: "GET" }, env);
    expect(response.status).toBe(200);
    await expect(response.json()).resolves.toMatchObject({
      connectors: {
        "google-calendar": {
          oauth: "brokered_confidential",
          session_ttl_seconds: 600,
          refresh_token_modes: ["handle"]
        }
      }
    });
  });

  it("starts a Google Calendar OAuth broker session with shared Google client id", async () => {
    const response = await app.request("/v1/oauth/google-calendar/start", { method: "POST" }, env);
    expect(response.status).toBe(200);
    const body = (await response.json()) as StartResponse;
    expect(body.connector).toBe("google-calendar");
    expect(body.client_id).toBe("google-client-id");
    const authorizationUrl = new URL(body.authorization_url);
    expect(`${authorizationUrl.origin}${authorizationUrl.pathname}`).toBe(
      "https://accounts.example.test/o/oauth2/v2/auth"
    );
    expect(authorizationUrl.searchParams.get("client_id")).toBe("google-client-id");
    expect(authorizationUrl.searchParams.get("response_type")).toBe("code");
    expect(authorizationUrl.searchParams.get("redirect_uri")).toBe(
      "http://localhost:8757/oauth/google-calendar/callback"
    );
    expect(authorizationUrl.searchParams.get("scope")?.split(" ").sort()).toEqual(
      [
        "openid",
        "email",
        "profile",
        "https://www.googleapis.com/auth/calendar.events"
      ].sort()
    );
    expect(authorizationUrl.searchParams.get("access_type")).toBe("offline");
    expect(authorizationUrl.searchParams.get("prompt")).toBe("consent");
    expect(authorizationUrl.searchParams.get("include_granted_scopes")).toBe("true");
    expect(body.redirect_uri).toBe("http://localhost:8757/oauth/google-calendar/callback");
    expect(body.session).toBeTruthy();
    expect(body.state).toBeTruthy();
  });

  it("exchanges a Google Calendar authorization code without exposing the raw refresh token in handle mode", async () => {
    const start = await startGoogleCalendarSession();
    const fetchMock = vi.fn(async (_input: RequestInfo | URL, _init?: RequestInit) =>
      Response.json({
        access_token: "calendar-access-token",
        refresh_token: "calendar-refresh-token",
        token_type: "Bearer",
        expires_in: 3600,
        scope: "openid email profile https://www.googleapis.com/auth/calendar.events",
        id_token: "calendar-id-token"
      })
    );
    globalThis.fetch = fetchMock as unknown as typeof fetch;

    const response = await app.request(
      "/v1/oauth/google-calendar/exchange",
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          session: start.session,
          state: start.state,
          code: "authorization-code",
          redirect_uri: "http://localhost:8757/oauth/google-calendar/callback"
        })
      },
      env
    );

    expect(response.status).toBe(200);
    const body = (await response.json()) as BrokerTokenResponse;
    expect(body.access_token).toBe("calendar-access-token");
    expect(body.scope).toBe("openid email profile https://www.googleapis.com/auth/calendar.events");
    expect(body.id_token).toBe("calendar-id-token");
    expect(body.workspace_id).toBe("primary");
    expect(body.workspace_name).toBe("Primary calendar");
    expect(body.refresh_token).toBeUndefined();
    expect(body.refresh_token_kind).toBe("handle");
    expect(body.refresh_token_handle).toMatch(/^locrh_v1\./);
    expect(fetchMock).toHaveBeenCalledWith(
      "https://oauth2.example.test/token",
      expect.objectContaining({
        method: "POST",
        headers: expect.objectContaining({
          "Content-Type": "application/x-www-form-urlencoded"
        })
      })
    );
    const requestBody = new URLSearchParams((fetchMock.mock.calls[0]?.[1] as RequestInit).body as string);
    expect(requestBody.get("client_id")).toBe("google-client-id");
    expect(requestBody.get("client_secret")).toBe("google-client-secret");
    expect(requestBody.get("grant_type")).toBe("authorization_code");
    expect(requestBody.get("code")).toBe("authorization-code");
    expect(requestBody.get("redirect_uri")).toBe("http://localhost:8757/oauth/google-calendar/callback");
  });
});

async function startGoogleCalendarSession() {
  const response = await app.request("/v1/oauth/google-calendar/start", { method: "POST" }, env);
  expect(response.status).toBe(200);
  return response.json() as Promise<StartResponse>;
}
