import { Hono } from "hono";
import { badRequest, configError, HttpError } from "./http/errors";
import {
  exchangeGoogleDocsCode,
  googleDocsAuthorizeUrl,
  refreshGoogleDocsToken,
  type GoogleDocsTokenResponse
} from "./oauth/google-docs";
import {
  exchangeGoogleCalendarCode,
  googleCalendarAuthorizeUrl,
  refreshGoogleCalendarToken,
  type GoogleCalendarTokenResponse
} from "./oauth/google-calendar";
import { exchangeGmailCode, gmailAuthorizeUrl, refreshGmailToken, type GmailTokenResponse } from "./oauth/gmail";
import { googleClientId } from "./oauth/google";
import { exchangeNotionCode, notionAuthorizeUrl, refreshNotionToken, type NotionTokenResponse } from "./oauth/notion";
import { exchangeSlackCode, refreshSlackToken, slackAuthorizeUrl, type SlackTokenResponse } from "./oauth/slack";
import { randomBase64Url, decryptJsonHandle, encryptJsonHandle } from "./security/crypto";
import {
  providerCallbackUri,
  validateGmailRedirectUri,
  validateGoogleCalendarRedirectUri,
  validateGoogleDocsRedirectUri,
  validateNotionRedirectUri,
  validateSlackRedirectUri
} from "./security/redirects";
import { nowSeconds, signSession, verifySession, type OAuthSessionPayloadV2 } from "./security/session";
import { ingestTelemetry } from "./telemetry";
import type { ApiErrorBody, BrokerEnv, ConnectorId } from "./types";

const SESSION_TTL_SECONDS = 10 * 60;
const OPERATIONAL_SECRET_MIN_LENGTH = 32;

interface StartRequest {
  redirect_uri?: string;
}

interface ExchangeRequest {
  session?: string;
  state?: string;
  code?: string;
  redirect_uri?: string;
}

interface RefreshRequest {
  refresh_token?: string;
  refresh_token_handle?: string;
}

interface RefreshHandlePayload {
  v: 1;
  connector: ConnectorId;
  refresh_token: string;
  issued_at: number;
}

const app = new Hono<{ Bindings: BrokerEnv }>();

app.get("/healthz", (c) => c.json({ ok: true }));

app.post("/v1/telemetry/batch", async (c) => {
  const result = await ingestTelemetry(c.req.raw, c.env);
  return c.json(result, 202);
});

app.get("/.well-known/loc-auth-broker", (c) =>
  c.json({
    issuer: "afs-oauth-broker",
    version: 1,
    connectors: {
      notion: {
        oauth: "brokered_confidential",
        session_ttl_seconds: SESSION_TTL_SECONDS,
        refresh_token_modes: [tokenMode(c.env)]
      },
      "google-docs": {
        oauth: "brokered_confidential",
        session_ttl_seconds: SESSION_TTL_SECONDS,
        refresh_token_modes: [tokenMode(c.env)]
      },
      "google-calendar": {
        oauth: "brokered_confidential",
        session_ttl_seconds: SESSION_TTL_SECONDS,
        refresh_token_modes: [tokenMode(c.env)]
      },
      gmail: {
        oauth: "brokered_confidential",
        session_ttl_seconds: SESSION_TTL_SECONDS,
        refresh_token_modes: [tokenMode(c.env)]
      },
      slack: {
        oauth: "brokered_confidential",
        session_ttl_seconds: SESSION_TTL_SECONDS,
        refresh_token_modes: [tokenMode(c.env)]
      }
    }
  })
);

app.get("/v1/oauth/:connector/callback", async (c) => {
  const connector = connectorFromParam(c.req.param("connector"));
  const state = requireString(c.req.query("state"), "state");
  const payload = await verifySession(
    state,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  if (payload.v !== 2 || payload.connector !== connector) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  if (payload.provider_redirect_uri !== providerCallbackUri(c.env, connector)) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker callback URI");
  }

  const requestUrl = new URL(c.req.url);
  if (!requestUrl.searchParams.get("code") && !requestUrl.searchParams.get("error")) {
    throw badRequest("invalid_oauth_callback", "OAuth callback must include code or error");
  }

  c.header("Cache-Control", "no-store");
  c.header("Referrer-Policy", "no-referrer");
  return c.redirect(callbackRedirectTarget(payload, state, requestUrl), 302);
});

app.post("/v1/oauth/notion/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  const clientRedirectUri = validateNotionRedirectUri(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/notion/callback"
  );
  const providerRedirectUri = providerCallbackUri(c.env, "notion");
  const session = await currentSession(c.env, "notion", clientRedirectUri, providerRedirectUri);
  return c.json({
    connector: "notion",
    client_id: c.env.LOCALITY_NOTION_CLIENT_ID,
    authorization_url: notionAuthorizeUrl(c.env, providerRedirectUri, session),
    redirect_uri: clientRedirectUri,
    provider_redirect_uri: providerRedirectUri,
    session,
    state: session,
    expires_in: SESSION_TTL_SECONDS
  });
});

app.post("/v1/oauth/notion/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const clientRedirectUri = validateNotionRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  const providerRedirectUri = providerCallbackUri(c.env, "notion");
  if (
    payload.v !== 2 ||
    payload.connector !== "notion" ||
    state !== session ||
    payload.client_redirect_uri !== clientRedirectUri ||
    payload.provider_redirect_uri !== providerRedirectUri
  ) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await exchangeNotionCode(c.env, code, payload.provider_redirect_uri);
  return c.json(await shapeNotionTokenResponse(c.env, token));
});

app.post("/v1/oauth/notion/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "notion", body);
  const token = await refreshNotionToken(c.env, refreshToken);
  return c.json(await shapeNotionTokenResponse(c.env, token));
});

app.post("/v1/oauth/google-docs/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  const clientRedirectUri = validateGoogleDocsRedirectUri(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/google-docs/callback"
  );
  const providerRedirectUri = providerCallbackUri(c.env, "google-docs");
  const session = await currentSession(c.env, "google-docs", clientRedirectUri, providerRedirectUri);
  return c.json({
    connector: "google-docs",
    client_id: googleClientId(c.env),
    authorization_url: googleDocsAuthorizeUrl(c.env, providerRedirectUri, session),
    redirect_uri: clientRedirectUri,
    provider_redirect_uri: providerRedirectUri,
    session,
    state: session,
    expires_in: SESSION_TTL_SECONDS
  });
});

app.post("/v1/oauth/google-docs/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const clientRedirectUri = validateGoogleDocsRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  const providerRedirectUri = providerCallbackUri(c.env, "google-docs");
  if (
    payload.v !== 2 ||
    payload.connector !== "google-docs" ||
    state !== session ||
    payload.client_redirect_uri !== clientRedirectUri ||
    payload.provider_redirect_uri !== providerRedirectUri
  ) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await exchangeGoogleDocsCode(c.env, code, payload.provider_redirect_uri);
  return c.json(await shapeGoogleDocsTokenResponse(c.env, token));
});

app.post("/v1/oauth/google-docs/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "google-docs", body);
  const token = await refreshGoogleDocsToken(c.env, refreshToken);
  return c.json(await shapeGoogleDocsTokenResponse(c.env, token));
});

app.post("/v1/oauth/google-calendar/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  const clientRedirectUri = validateGoogleCalendarRedirectUri(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/google-calendar/callback"
  );
  const providerRedirectUri = providerCallbackUri(c.env, "google-calendar");
  const session = await currentSession(c.env, "google-calendar", clientRedirectUri, providerRedirectUri);
  return c.json({
    connector: "google-calendar",
    client_id: googleClientId(c.env),
    authorization_url: googleCalendarAuthorizeUrl(c.env, providerRedirectUri, session),
    redirect_uri: clientRedirectUri,
    provider_redirect_uri: providerRedirectUri,
    session,
    state: session,
    expires_in: SESSION_TTL_SECONDS
  });
});

app.post("/v1/oauth/google-calendar/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const clientRedirectUri = validateGoogleCalendarRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  const providerRedirectUri = providerCallbackUri(c.env, "google-calendar");
  if (
    payload.v !== 2 ||
    payload.connector !== "google-calendar" ||
    state !== session ||
    payload.client_redirect_uri !== clientRedirectUri ||
    payload.provider_redirect_uri !== providerRedirectUri
  ) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await exchangeGoogleCalendarCode(c.env, code, payload.provider_redirect_uri);
  return c.json(await shapeGoogleCalendarTokenResponse(c.env, token));
});

app.post("/v1/oauth/google-calendar/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "google-calendar", body);
  const token = await refreshGoogleCalendarToken(c.env, refreshToken);
  return c.json(await shapeGoogleCalendarTokenResponse(c.env, token));
});

app.post("/v1/oauth/gmail/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  const clientRedirectUri = validateGmailRedirectUri(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/gmail/callback"
  );
  const providerRedirectUri = providerCallbackUri(c.env, "gmail");
  const session = await currentSession(c.env, "gmail", clientRedirectUri, providerRedirectUri);
  return c.json({
    connector: "gmail",
    client_id: googleClientId(c.env),
    authorization_url: gmailAuthorizeUrl(c.env, providerRedirectUri, session),
    redirect_uri: clientRedirectUri,
    provider_redirect_uri: providerRedirectUri,
    session,
    state: session,
    expires_in: SESSION_TTL_SECONDS
  });
});

app.post("/v1/oauth/gmail/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const clientRedirectUri = validateGmailRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  const providerRedirectUri = providerCallbackUri(c.env, "gmail");
  if (
    payload.v !== 2 ||
    payload.connector !== "gmail" ||
    state !== session ||
    payload.client_redirect_uri !== clientRedirectUri ||
    payload.provider_redirect_uri !== providerRedirectUri
  ) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await exchangeGmailCode(c.env, code, payload.provider_redirect_uri);
  return c.json(await shapeGmailTokenResponse(c.env, token));
});

app.post("/v1/oauth/gmail/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "gmail", body);
  const token = await refreshGmailToken(c.env, refreshToken);
  return c.json(await shapeGmailTokenResponse(c.env, token));
});

app.post("/v1/oauth/slack/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  const clientRedirectUri = validateSlackRedirectUri(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/slack/callback"
  );
  const providerRedirectUri = providerCallbackUri(c.env, "slack");
  const session = await currentSession(c.env, "slack", clientRedirectUri, providerRedirectUri);
  return c.json({
    connector: "slack",
    client_id: c.env.LOCALITY_SLACK_CLIENT_ID,
    authorization_url: slackAuthorizeUrl(c.env, providerRedirectUri, session),
    redirect_uri: clientRedirectUri,
    provider_redirect_uri: providerRedirectUri,
    session,
    state: session,
    expires_in: SESSION_TTL_SECONDS
  });
});

app.post("/v1/oauth/slack/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const clientRedirectUri = validateSlackRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  const providerRedirectUri = providerCallbackUri(c.env, "slack");
  if (
    payload.v !== 2 ||
    payload.connector !== "slack" ||
    state !== session ||
    payload.client_redirect_uri !== clientRedirectUri ||
    payload.provider_redirect_uri !== providerRedirectUri
  ) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await exchangeSlackCode(c.env, code, payload.provider_redirect_uri);
  return c.json(await shapeSlackTokenResponse(c.env, token));
});

app.post("/v1/oauth/slack/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "slack", body);
  const token = await refreshSlackToken(c.env, refreshToken);
  return c.json(await shapeSlackTokenResponse(c.env, token));
});

app.onError((error, c) => {
  const httpError = error instanceof HttpError ? error : new HttpError(500, "internal_error", "internal server error");
  const body: ApiErrorBody = {
    error: {
      code: httpError.code,
      message: httpError.message
    }
  };
  return c.json(body, httpError.status as never);
});

async function shapeNotionTokenResponse(env: BrokerEnv, token: NotionTokenResponse) {
  const refresh = await shapeRefreshToken(env, "notion", token.refresh_token);
  return {
    connector: "notion",
    access_token: token.access_token,
    token_type: token.token_type,
    expires_in: token.expires_in,
    workspace_id: token.workspace_id,
    workspace_name: token.workspace_name,
    workspace_icon: token.workspace_icon,
    bot_id: token.bot_id,
    owner: token.owner,
    duplicated_template_id: token.duplicated_template_id,
    ...refresh
  };
}

async function shapeGoogleDocsTokenResponse(env: BrokerEnv, token: GoogleDocsTokenResponse) {
  const refresh = await shapeRefreshToken(env, "google-docs", token.refresh_token);
  return {
    connector: "google-docs",
    access_token: token.access_token,
    token_type: token.token_type,
    expires_in: token.expires_in,
    scope: token.scope,
    id_token: token.id_token,
    ...refresh
  };
}

async function shapeGoogleCalendarTokenResponse(env: BrokerEnv, token: GoogleCalendarTokenResponse) {
  const refresh = await shapeRefreshToken(env, "google-calendar", token.refresh_token);
  return {
    connector: "google-calendar",
    access_token: token.access_token,
    token_type: token.token_type,
    expires_in: token.expires_in,
    scope: token.scope,
    id_token: token.id_token,
    workspace_id: "primary",
    workspace_name: "Primary calendar",
    ...refresh
  };
}

async function shapeGmailTokenResponse(env: BrokerEnv, token: GmailTokenResponse) {
  const refresh = await shapeRefreshToken(env, "gmail", token.refresh_token);
  return {
    connector: "gmail",
    access_token: token.access_token,
    token_type: token.token_type,
    expires_in: token.expires_in,
    scope: token.scope,
    id_token: token.id_token,
    ...refresh
  };
}

async function shapeSlackTokenResponse(env: BrokerEnv, token: SlackTokenResponse) {
  const refresh = await shapeRefreshToken(env, "slack", token.refresh_token);
  const scopes = token.scope?.split(/[,\s]+/).filter(Boolean) ?? [];
  const workspace = token.team ?? token.enterprise;
  return {
    connector: "slack",
    access_token: token.access_token,
    token_type: token.token_type,
    expires_in: token.expires_in,
    scopes,
    account_id: workspace?.id,
    account_label: workspace?.name,
    workspace_id: workspace?.id,
    workspace_name: workspace?.name,
    bot_id: token.bot_user_id,
    ...refresh
  };
}

function connectorFromParam(value: string): ConnectorId {
  if (
    value === "notion" ||
    value === "google-docs" ||
    value === "google-calendar" ||
    value === "gmail" ||
    value === "slack"
  ) {
    return value;
  }
  throw badRequest("unknown_connector", "OAuth connector is not supported");
}

async function currentSession(
  env: BrokerEnv,
  connector: ConnectorId,
  clientRedirectUri: string,
  providerRedirectUri: string
): Promise<string> {
  const now = nowSeconds();
  return signSession(
    {
      v: 2,
      connector,
      state_nonce: randomBase64Url(),
      client_redirect_uri: clientRedirectUri,
      provider_redirect_uri: providerRedirectUri,
      iat: now,
      exp: now + SESSION_TTL_SECONDS,
      nonce: randomBase64Url()
    },
    requireOperationalSecret(env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
}

function callbackRedirectTarget(payload: OAuthSessionPayloadV2, state: string, requestUrl: URL): string {
  const target = new URL(payload.client_redirect_uri);
  const code = requestUrl.searchParams.get("code");
  if (code) {
    target.searchParams.set("code", boundedCallbackValue(code, "code"));
  }
  const error = requestUrl.searchParams.get("error");
  if (error) {
    target.searchParams.set("error", boundedCallbackValue(error, "error"));
  }
  const errorDescription = requestUrl.searchParams.get("error_description");
  if (errorDescription) {
    target.searchParams.set("error_description", boundedCallbackValue(errorDescription, "error_description"));
  }
  const errorUri = requestUrl.searchParams.get("error_uri");
  if (errorUri) {
    target.searchParams.set("error_uri", boundedCallbackValue(errorUri, "error_uri"));
  }
  target.searchParams.set("state", state);
  return target.toString();
}

function boundedCallbackValue(value: string, field: string): string {
  if (value.length > 4096) {
    throw badRequest("invalid_oauth_callback", `${field} is too large`);
  }
  return value;
}

async function shapeRefreshToken(env: BrokerEnv, connector: ConnectorId, refreshToken: string | undefined) {
  if (!refreshToken) {
    return {};
  }
  if (tokenMode(env) === "raw") {
    return {
      refresh_token_kind: "raw",
      refresh_token: refreshToken
    };
  }
  const secret = requireOperationalSecret(env.LOCALITY_REFRESH_HANDLE_KEY, "LOCALITY_REFRESH_HANDLE_KEY");
  const handle = await encryptJsonHandle(
    {
      v: 1,
      connector,
      refresh_token: refreshToken,
      issued_at: nowSeconds()
    } satisfies RefreshHandlePayload,
    secret
  );
  return {
    refresh_token_kind: "handle",
    refresh_token_handle: handle
  };
}

async function resolveRefreshToken(env: BrokerEnv, connector: ConnectorId, body: RefreshRequest): Promise<string> {
  if (body.refresh_token_handle) {
    try {
      const payload = await decryptJsonHandle<RefreshHandlePayload>(
        body.refresh_token_handle,
        requireOperationalSecret(env.LOCALITY_REFRESH_HANDLE_KEY, "LOCALITY_REFRESH_HANDLE_KEY")
      );
      if (payload.v !== 1 || payload.connector !== connector) {
        throw new Error("invalid refresh handle payload");
      }
      return payload.refresh_token;
    } catch {
      throw badRequest("invalid_refresh_handle", "refresh_token_handle is invalid");
    }
  }
  if (tokenMode(env) !== "raw") {
    throw badRequest("missing_refresh_handle", "refresh_token_handle is required");
  }
  return requireString(body.refresh_token, "refresh_token");
}

async function optionalJson<T>(request: Request): Promise<T> {
  if (!request.headers.get("content-type")?.includes("application/json")) {
    return {} as T;
  }
  return requiredJson<T>(request);
}

async function requiredJson<T>(request: Request): Promise<T> {
  try {
    return (await request.json()) as T;
  } catch {
    throw badRequest("invalid_json", "request body must be valid JSON");
  }
}

function requireString(value: string | undefined, field: string): string {
  if (!value || value.trim() === "") {
    throw badRequest("missing_field", `${field} is required`);
  }
  return value;
}

function tokenMode(env: BrokerEnv): "handle" | "raw" {
  const mode = env.LOCALITY_TOKEN_MODE ?? (env.LOCALITY_REFRESH_HANDLE_KEY ? "handle" : "raw");
  if (mode !== "handle" && mode !== "raw") {
    throw configError("LOCALITY_TOKEN_MODE must be either handle or raw");
  }
  return mode;
}

function requireOperationalSecret(value: string | undefined, name: string): string {
  if (!value || value.length < OPERATIONAL_SECRET_MIN_LENGTH) {
    throw configError(`${name} must be configured`);
  }
  return value;
}

export default app;
