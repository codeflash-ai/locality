import { Hono } from "hono";
import { badRequest, configError, HttpError } from "./http/errors";
import {
  exchangeGoogleDocsCode,
  googleDocsAuthorizeUrl,
  refreshGoogleDocsToken,
  type GoogleDocsTokenResponse
} from "./oauth/google-docs";
import { exchangeGmailCode, gmailAuthorizeUrl, refreshGmailToken, type GmailTokenResponse } from "./oauth/gmail";
import { googleClientId } from "./oauth/google";
import { exchangeNotionCode, notionAuthorizeUrl, refreshNotionToken, type NotionTokenResponse } from "./oauth/notion";
import { randomBase64Url, decryptJsonHandle, encryptJsonHandle } from "./security/crypto";
import { validateGmailRedirectUri, validateGoogleDocsRedirectUri, validateNotionRedirectUri } from "./security/redirects";
import { nowSeconds, signSession, verifySession } from "./security/session";
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
      gmail: {
        oauth: "brokered_confidential",
        session_ttl_seconds: SESSION_TTL_SECONDS,
        refresh_token_modes: [tokenMode(c.env)]
      }
    }
  })
);

app.post("/v1/oauth/notion/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  const redirectUri = validateNotionRedirectUri(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/notion/callback"
  );
  const now = nowSeconds();
  const state = randomBase64Url();
  const session = await signSession(
    {
      v: 1,
      connector: "notion",
      state,
      redirect_uri: redirectUri,
      iat: now,
      exp: now + SESSION_TTL_SECONDS,
      nonce: randomBase64Url()
    },
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  return c.json({
    connector: "notion",
    client_id: c.env.LOCALITY_NOTION_CLIENT_ID,
    authorization_url: notionAuthorizeUrl(c.env, redirectUri, state),
    redirect_uri: redirectUri,
    session,
    state,
    expires_in: SESSION_TTL_SECONDS
  });
});

app.post("/v1/oauth/notion/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const redirectUri = validateNotionRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  if (payload.connector !== "notion" || payload.state !== state || payload.redirect_uri !== redirectUri) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await exchangeNotionCode(c.env, code, redirectUri);
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
  const redirectUri = validateGoogleDocsRedirectUri(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/google-docs/callback"
  );
  const now = nowSeconds();
  const state = randomBase64Url();
  const session = await signSession(
    {
      v: 1,
      connector: "google-docs",
      state,
      redirect_uri: redirectUri,
      iat: now,
      exp: now + SESSION_TTL_SECONDS,
      nonce: randomBase64Url()
    },
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  return c.json({
    connector: "google-docs",
    client_id: googleClientId(c.env),
    authorization_url: googleDocsAuthorizeUrl(c.env, redirectUri, state),
    redirect_uri: redirectUri,
    session,
    state,
    expires_in: SESSION_TTL_SECONDS
  });
});

app.post("/v1/oauth/google-docs/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const redirectUri = validateGoogleDocsRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  if (payload.connector !== "google-docs" || payload.state !== state || payload.redirect_uri !== redirectUri) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await exchangeGoogleDocsCode(c.env, code, redirectUri);
  return c.json(await shapeGoogleDocsTokenResponse(c.env, token));
});

app.post("/v1/oauth/google-docs/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "google-docs", body);
  const token = await refreshGoogleDocsToken(c.env, refreshToken);
  return c.json(await shapeGoogleDocsTokenResponse(c.env, token));
});

app.post("/v1/oauth/gmail/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  const redirectUri = validateGmailRedirectUri(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/gmail/callback"
  );
  const now = nowSeconds();
  const state = randomBase64Url();
  const session = await signSession(
    {
      v: 1,
      connector: "gmail",
      state,
      redirect_uri: redirectUri,
      iat: now,
      exp: now + SESSION_TTL_SECONDS,
      nonce: randomBase64Url()
    },
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  return c.json({
    connector: "gmail",
    client_id: googleClientId(c.env),
    authorization_url: gmailAuthorizeUrl(c.env, redirectUri, state),
    redirect_uri: redirectUri,
    session,
    state,
    expires_in: SESSION_TTL_SECONDS
  });
});

app.post("/v1/oauth/gmail/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const redirectUri = validateGmailRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  if (payload.connector !== "gmail" || payload.state !== state || payload.redirect_uri !== redirectUri) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await exchangeGmailCode(c.env, code, redirectUri);
  return c.json(await shapeGmailTokenResponse(c.env, token));
});

app.post("/v1/oauth/gmail/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "gmail", body);
  const token = await refreshGmailToken(c.env, refreshToken);
  return c.json(await shapeGmailTokenResponse(c.env, token));
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
