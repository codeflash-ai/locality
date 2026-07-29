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
  hostedConnectorCallbackUri,
  validateGmailExchangeRedirectUri,
  validateGmailRedirectUri,
  validateGoogleCalendarExchangeRedirectUri,
  validateGoogleCalendarRedirectUri,
  validateGoogleDocsExchangeRedirectUri,
  validateGoogleDocsRedirectUri,
  validateNotionExchangeRedirectUri,
  validateNotionRedirectUri,
  validateSlackExchangeRedirectUri,
  validateSlackRedirectUri
} from "./security/redirects";
import {
  nowSeconds,
  signLocalHandoffState,
  signSession,
  verifyLocalHandoffState,
  verifySession
} from "./security/session";
import type { ApiErrorBody, BrokerEnv, ConnectorId } from "./types";

const SESSION_TTL_SECONDS = 10 * 60;
const OPERATIONAL_SECRET_MIN_LENGTH = 32;

interface StartRequest {
  redirect_uri?: string;
}

interface StartRedirects {
  localRedirectUri: string;
  authorizationRedirectUri: string;
  exchangeRedirectUri: string;
  hostedHandoff: boolean;
}

interface OAuthConnectorRuntime<TokenResponse> {
  connector: ConnectorId;
  defaultLocalRedirectUri: string;
  clientId(env: BrokerEnv): string;
  authorizeUrl(env: BrokerEnv, redirectUri: string, state: string): string;
  validateLocalRedirectUri(env: BrokerEnv, redirectUri: string): string;
  validateExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string;
  exchangeCode(env: BrokerEnv, code: string, redirectUri: string): Promise<TokenResponse>;
  shapeTokenResponse(env: BrokerEnv, token: TokenResponse): Promise<unknown>;
}

interface ExchangeRequest {
  session?: string;
  state?: string;
  code?: string;
  redirect_uri?: string;
}

interface HostedCallbackQuery {
  state?: string;
  code?: string;
  error?: string;
  error_description?: string;
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

const oauthConnectors: Record<ConnectorId, OAuthConnectorRuntime<unknown>> = {
  notion: {
    connector: "notion",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/notion/callback",
    clientId: (env) => env.LOCALITY_NOTION_CLIENT_ID,
    authorizeUrl: notionAuthorizeUrl,
    validateLocalRedirectUri: validateNotionRedirectUri,
    validateExchangeRedirectUri: validateNotionExchangeRedirectUri,
    exchangeCode: exchangeNotionCode,
    shapeTokenResponse: (env, token) => shapeNotionTokenResponse(env, token as NotionTokenResponse)
  },
  "google-docs": {
    connector: "google-docs",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/google-docs/callback",
    clientId: googleClientId,
    authorizeUrl: googleDocsAuthorizeUrl,
    validateLocalRedirectUri: validateGoogleDocsRedirectUri,
    validateExchangeRedirectUri: validateGoogleDocsExchangeRedirectUri,
    exchangeCode: exchangeGoogleDocsCode,
    shapeTokenResponse: (env, token) => shapeGoogleDocsTokenResponse(env, token as GoogleDocsTokenResponse)
  },
  "google-calendar": {
    connector: "google-calendar",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/google-calendar/callback",
    clientId: googleClientId,
    authorizeUrl: googleCalendarAuthorizeUrl,
    validateLocalRedirectUri: validateGoogleCalendarRedirectUri,
    validateExchangeRedirectUri: validateGoogleCalendarExchangeRedirectUri,
    exchangeCode: exchangeGoogleCalendarCode,
    shapeTokenResponse: (env, token) => shapeGoogleCalendarTokenResponse(env, token as GoogleCalendarTokenResponse)
  },
  gmail: {
    connector: "gmail",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/gmail/callback",
    clientId: googleClientId,
    authorizeUrl: gmailAuthorizeUrl,
    validateLocalRedirectUri: validateGmailRedirectUri,
    validateExchangeRedirectUri: validateGmailExchangeRedirectUri,
    exchangeCode: exchangeGmailCode,
    shapeTokenResponse: (env, token) => shapeGmailTokenResponse(env, token as GmailTokenResponse)
  },
  slack: {
    connector: "slack",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/slack/callback",
    clientId: (env) => requireConfiguredString(env.LOCALITY_SLACK_CLIENT_ID, "LOCALITY_SLACK_CLIENT_ID"),
    authorizeUrl: slackAuthorizeUrl,
    validateLocalRedirectUri: validateSlackRedirectUri,
    validateExchangeRedirectUri: validateSlackExchangeRedirectUri,
    exchangeCode: exchangeSlackCode,
    shapeTokenResponse: (env, token) => shapeSlackTokenResponse(env, token as SlackTokenResponse)
  }
};

app.post("/v1/oauth/notion/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  return c.json(await startOAuthConnector(c.env, "notion", body));
});

app.get("/v1/oauth/notion/callback", async (c) => {
  return hostedCallbackResponse(c.env, "notion", validateNotionRedirectUri, c.req.query() as HostedCallbackQuery);
});

app.post("/v1/oauth/notion/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  return c.json(await exchangeOAuthConnector(c.env, "notion", body));
});

app.post("/v1/oauth/notion/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "notion", body);
  const token = await refreshNotionToken(c.env, refreshToken);
  return c.json(await shapeNotionTokenResponse(c.env, token));
});

app.post("/v1/oauth/google-docs/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  return c.json(await startOAuthConnector(c.env, "google-docs", body));
});

app.get("/v1/oauth/google-docs/callback", async (c) => {
  return hostedCallbackResponse(
    c.env,
    "google-docs",
    validateGoogleDocsRedirectUri,
    c.req.query() as HostedCallbackQuery
  );
});

app.post("/v1/oauth/google-docs/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  return c.json(await exchangeOAuthConnector(c.env, "google-docs", body));
});

app.post("/v1/oauth/google-docs/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "google-docs", body);
  const token = await refreshGoogleDocsToken(c.env, refreshToken);
  return c.json(await shapeGoogleDocsTokenResponse(c.env, token));
});

app.post("/v1/oauth/google-calendar/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  return c.json(await startOAuthConnector(c.env, "google-calendar", body));
});

app.get("/v1/oauth/google-calendar/callback", async (c) => {
  return hostedCallbackResponse(
    c.env,
    "google-calendar",
    validateGoogleCalendarRedirectUri,
    c.req.query() as HostedCallbackQuery
  );
});

app.post("/v1/oauth/google-calendar/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  return c.json(await exchangeOAuthConnector(c.env, "google-calendar", body));
});

app.post("/v1/oauth/google-calendar/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "google-calendar", body);
  const token = await refreshGoogleCalendarToken(c.env, refreshToken);
  return c.json(await shapeGoogleCalendarTokenResponse(c.env, token));
});

app.post("/v1/oauth/gmail/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  return c.json(await startOAuthConnector(c.env, "gmail", body));
});

app.get("/v1/oauth/gmail/callback", async (c) => {
  return hostedCallbackResponse(c.env, "gmail", validateGmailRedirectUri, c.req.query() as HostedCallbackQuery);
});

app.post("/v1/oauth/gmail/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  return c.json(await exchangeOAuthConnector(c.env, "gmail", body));
});

app.post("/v1/oauth/gmail/refresh", async (c) => {
  const body = await requiredJson<RefreshRequest>(c.req.raw);
  const refreshToken = await resolveRefreshToken(c.env, "gmail", body);
  const token = await refreshGmailToken(c.env, refreshToken);
  return c.json(await shapeGmailTokenResponse(c.env, token));
});

app.post("/v1/oauth/slack/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  return c.json(await startOAuthConnector(c.env, "slack", body));
});

app.get("/v1/oauth/slack/callback", async (c) => {
  return hostedCallbackResponse(c.env, "slack", validateSlackRedirectUri, c.req.query() as HostedCallbackQuery);
});

app.post("/v1/oauth/slack/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  return c.json(await exchangeOAuthConnector(c.env, "slack", body));
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

async function startOAuthConnector(env: BrokerEnv, connector: ConnectorId, body: StartRequest) {
  const runtime = oauthConnectors[connector];
  const redirects = startRedirects(
    env,
    connector,
    runtime,
    body.redirect_uri ?? runtime.defaultLocalRedirectUri
  );
  const now = nowSeconds();
  const secret = requireOperationalSecret(env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET");
  const state = redirects.hostedHandoff
    ? await signLocalHandoffState(
        {
          v: 1,
          kind: "local_handoff",
          connector,
          local_redirect_uri: redirects.localRedirectUri,
          provider_redirect_uri: redirects.authorizationRedirectUri,
          iat: now,
          exp: now + SESSION_TTL_SECONDS,
          nonce: randomBase64Url()
        },
        secret
      )
    : randomBase64Url();
  const session = await signSession(
    {
      v: 1,
      connector,
      state,
      redirect_uri: redirects.exchangeRedirectUri,
      iat: now,
      exp: now + SESSION_TTL_SECONDS,
      nonce: randomBase64Url()
    },
    secret
  );
  return {
    connector: runtime.connector,
    client_id: runtime.clientId(env),
    authorization_url: runtime.authorizeUrl(env, redirects.authorizationRedirectUri, state),
    redirect_uri: redirects.localRedirectUri,
    authorization_redirect_uri: redirects.authorizationRedirectUri,
    exchange_redirect_uri: redirects.exchangeRedirectUri,
    session,
    state,
    expires_in: SESSION_TTL_SECONDS
  };
}

async function hostedCallbackResponse(
  env: BrokerEnv,
  connector: ConnectorId,
  validateLocalRedirectUri: (env: BrokerEnv, redirectUri: string) => string,
  query: HostedCallbackQuery
): Promise<Response> {
  const state = callbackString(query.state, "state", 8192);
  const payload = await verifyLocalHandoffState(
    state,
    requireOperationalSecret(env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  if (payload.connector !== connector) {
    throw badRequest("invalid_state", "OAuth state connector is invalid");
  }
  const expectedProviderRedirectUri = hostedConnectorCallbackUri(env, connector);
  if (!expectedProviderRedirectUri || payload.provider_redirect_uri !== expectedProviderRedirectUri) {
    throw badRequest("invalid_state", "OAuth state provider redirect is invalid");
  }
  const localRedirectUri = validateLocalRedirectUri(env, payload.local_redirect_uri);
  const redirect = new URL(localRedirectUri);
  redirect.searchParams.set("state", state);
  const providerError = optionalCallbackString(query.error, "error", 256);
  if (providerError) {
    redirect.searchParams.set("error", providerError);
    const description = optionalCallbackString(query.error_description, "error_description", 1024);
    if (description) {
      redirect.searchParams.set("error_description", description);
    }
    return localCallbackRedirect(redirect.toString());
  }
  redirect.searchParams.set("code", callbackString(query.code, "code", 4096));
  return localCallbackRedirect(redirect.toString());
}

async function exchangeOAuthConnector(env: BrokerEnv, connector: ConnectorId, body: ExchangeRequest): Promise<unknown> {
  const runtime = oauthConnectors[connector];
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const redirectUri = runtime.validateExchangeRedirectUri(env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  if (payload.connector !== connector || payload.state !== state || payload.redirect_uri !== redirectUri) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await runtime.exchangeCode(env, code, redirectUri);
  return runtime.shapeTokenResponse(env, token);
}

function startRedirects(
  env: BrokerEnv,
  connector: ConnectorId,
  runtime: OAuthConnectorRuntime<unknown>,
  requestedRedirectUri: string
): StartRedirects {
  const localRedirectUri = runtime.validateLocalRedirectUri(env, requestedRedirectUri);
  const hostedCallbackUri = hostedConnectorCallbackUri(env, connector);
  if (!hostedCallbackUri) {
    return {
      localRedirectUri,
      authorizationRedirectUri: localRedirectUri,
      exchangeRedirectUri: localRedirectUri,
      hostedHandoff: false
    };
  }
  return {
    localRedirectUri,
    authorizationRedirectUri: hostedCallbackUri,
    exchangeRedirectUri: hostedCallbackUri,
    hostedHandoff: true
  };
}

function callbackString(value: string | undefined, field: string, maxBytes: number): string {
  if (!value || value.trim() === "" || new TextEncoder().encode(value).byteLength > maxBytes || hasControlCharacter(value)) {
    throw badRequest("invalid_callback", `${field} is invalid`);
  }
  return value;
}

function optionalCallbackString(value: string | undefined, field: string, maxBytes: number): string | undefined {
  if (value === undefined) {
    return undefined;
  }
  return callbackString(value, field, maxBytes);
}

function hasControlCharacter(value: string): boolean {
  for (const character of value) {
    const codePoint = character.charCodeAt(0);
    if (codePoint < 0x20 || codePoint === 0x7f) {
      return true;
    }
  }
  return false;
}

function localCallbackRedirect(location: string): Response {
  return new Response(null, {
    status: 303,
    headers: {
      "Cache-Control": "no-store",
      "Referrer-Policy": "no-referrer",
      Location: location
    }
  });
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

function requireConfiguredString(value: string | undefined, name: string): string {
  if (!value) {
    throw configError(`${name} must be configured`);
  }
  return value;
}

export default app;
