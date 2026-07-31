# All OAuth Hosted Handoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend the hosted OAuth callback handoff from Notion to Google Docs, Google Calendar, Gmail, and Slack while keeping local credential storage.

**Architecture:** The Worker gets one generic hosted-handoff path per connector: `/start` signs local handoff state, provider callbacks verify that state, then redirect back to the connector's local loopback listener. Rust shared broker responses gain optional hosted redirect fields, and CLI/desktop consumers listen locally but exchange with `exchange_redirect_uri`.

**Tech Stack:** Cloudflare Workers/Hono/TypeScript/Vitest, Rust crates `locality-connector`, `loc-cli`, connector crates, Tauri desktop command code, existing Locality OAuth broker abstractions.

---

## File Structure

- `apps/oauth-service/src/types.ts`: add hosted callback env vars for Google Docs, Google Calendar, Gmail, and Slack.
- `apps/oauth-service/src/security/redirects.ts`: replace Notion-only hosted validation with connector-aware hosted callback and exchange redirect helpers.
- `apps/oauth-service/src/app.ts`: replace Notion-only start/callback helpers with connector-aware helpers used by all five OAuth connectors.
- `apps/oauth-service/test/app.test.ts`: add parameterized hosted start/callback/exchange tests for every connector.
- `crates/locality-connector/src/oauth_broker.rs`: add optional hosted redirect fields and helper accessors to the shared broker start response.
- `crates/loc-cli/src/commands.rs`: use local listener redirect and exchange redirect helpers for Google Docs, Google Calendar, Gmail, and Slack.
- `apps/desktop/src-tauri/src/main.rs`: use the same local listener / exchange redirect split for desktop Google Docs, Google Calendar, Gmail, and Slack.
- `crates/loc-cli/tests/connect.rs`: make fake non-Notion broker exchanges configurable and add hosted exchange redirect storage tests.
- `apps/oauth-service/wrangler.toml`: configure hosted callback URIs for every OAuth connector and keep local loopback allowlists separate.
- `apps/oauth-service/README.md`, `apps/oauth-service/docs/deployment.md`, `apps/oauth-service/docs/security.md`, `docs/cli.md`, `docs-site/cli-reference.mdx`, `docs/notion-connector.md`: update docs from Notion-only to all OAuth connectors.

---

### Task 1: Generalize Worker Hosted Handoff To Every Connector

**Files:**
- Modify: `apps/oauth-service/src/types.ts`
- Modify: `apps/oauth-service/src/security/redirects.ts`
- Modify: `apps/oauth-service/src/app.ts`
- Modify: `apps/oauth-service/test/app.test.ts`

- [ ] **Step 1: Add failing hosted-flow tests for every connector**

In `apps/oauth-service/test/app.test.ts`, extend `StartResponse`:

```ts
interface StartResponse {
  connector: string;
  client_id: string;
  authorization_url: string;
  redirect_uri: string;
  authorization_redirect_uri?: string;
  exchange_redirect_uri?: string;
  session: string;
  state: string;
}
```

Add a connector table near the existing `hostedNotionCallbackUri` constant:

```ts
const brokerOrigin = "https://afs-oauth-broker.saurabh-b07.workers.dev";

const hostedConnectorCases = [
  {
    connector: "notion",
    startPath: "/v1/oauth/notion/start",
    callbackPath: "/v1/oauth/notion/callback",
    exchangePath: "/v1/oauth/notion/exchange",
    localRedirectUri: "http://localhost:8757/oauth/notion/callback",
    hostedCallbackUri: `${brokerOrigin}/v1/oauth/notion/callback`,
    env: { LOCALITY_NOTION_HOSTED_CALLBACK_URI: `${brokerOrigin}/v1/oauth/notion/callback` },
    tokenResponse: {
      access_token: "notion-access-token",
      refresh_token: "notion-refresh-token",
      token_type: "bearer",
      expires_in: 3600,
      workspace_id: "workspace-id"
    },
    upstreamBody(input: RequestInfo | URL, init?: RequestInit) {
      return JSON.parse((init as RequestInit).body as string) as Record<string, string>;
    }
  },
  {
    connector: "google-docs",
    startPath: "/v1/oauth/google-docs/start",
    callbackPath: "/v1/oauth/google-docs/callback",
    exchangePath: "/v1/oauth/google-docs/exchange",
    localRedirectUri: "http://localhost:8757/oauth/google-docs/callback",
    hostedCallbackUri: `${brokerOrigin}/v1/oauth/google-docs/callback`,
    env: { LOCALITY_GOOGLE_DOCS_HOSTED_CALLBACK_URI: `${brokerOrigin}/v1/oauth/google-docs/callback` },
    tokenResponse: {
      access_token: "google-docs-access-token",
      refresh_token: "google-docs-refresh-token",
      token_type: "Bearer",
      expires_in: 3600,
      scope:
        "openid email profile https://www.googleapis.com/auth/documents https://www.googleapis.com/auth/drive.file https://www.googleapis.com/auth/drive.metadata"
    },
    upstreamBody(input: RequestInfo | URL, init?: RequestInit) {
      return Object.fromEntries(new URLSearchParams((init as RequestInit).body as string));
    }
  },
  {
    connector: "google-calendar",
    startPath: "/v1/oauth/google-calendar/start",
    callbackPath: "/v1/oauth/google-calendar/callback",
    exchangePath: "/v1/oauth/google-calendar/exchange",
    localRedirectUri: "http://localhost:8757/oauth/google-calendar/callback",
    hostedCallbackUri: `${brokerOrigin}/v1/oauth/google-calendar/callback`,
    env: { LOCALITY_GOOGLE_CALENDAR_HOSTED_CALLBACK_URI: `${brokerOrigin}/v1/oauth/google-calendar/callback` },
    tokenResponse: {
      access_token: "calendar-access-token",
      refresh_token: "calendar-refresh-token",
      token_type: "Bearer",
      expires_in: 3600,
      scope: "openid email profile https://www.googleapis.com/auth/calendar.events",
      id_token: "calendar-id-token"
    },
    upstreamBody(input: RequestInfo | URL, init?: RequestInit) {
      return Object.fromEntries(new URLSearchParams((init as RequestInit).body as string));
    }
  },
  {
    connector: "gmail",
    startPath: "/v1/oauth/gmail/start",
    callbackPath: "/v1/oauth/gmail/callback",
    exchangePath: "/v1/oauth/gmail/exchange",
    localRedirectUri: "http://localhost:8757/oauth/gmail/callback",
    hostedCallbackUri: `${brokerOrigin}/v1/oauth/gmail/callback`,
    env: { LOCALITY_GMAIL_HOSTED_CALLBACK_URI: `${brokerOrigin}/v1/oauth/gmail/callback` },
    tokenResponse: {
      access_token: "gmail-access-token",
      refresh_token: "gmail-refresh-token",
      token_type: "Bearer",
      expires_in: 3600,
      scope:
        "openid email profile https://www.googleapis.com/auth/gmail.readonly https://www.googleapis.com/auth/gmail.compose",
      id_token: "gmail-id-token"
    },
    upstreamBody(input: RequestInfo | URL, init?: RequestInit) {
      return Object.fromEntries(new URLSearchParams((init as RequestInit).body as string));
    }
  },
  {
    connector: "slack",
    startPath: "/v1/oauth/slack/start",
    callbackPath: "/v1/oauth/slack/callback",
    exchangePath: "/v1/oauth/slack/exchange",
    localRedirectUri: "http://localhost:8757/oauth/slack/callback",
    hostedCallbackUri: `${brokerOrigin}/v1/oauth/slack/callback`,
    env: { LOCALITY_SLACK_HOSTED_CALLBACK_URI: `${brokerOrigin}/v1/oauth/slack/callback` },
    tokenResponse: {
      ok: true,
      access_token: "xoxb-access-token",
      refresh_token: "slack-refresh-token",
      token_type: "bot",
      expires_in: 43200,
      scope:
        "channels:read,channels:history,groups:read,groups:history,im:read,im:history,mpim:read,mpim:history,users:read,team:read,files:read,channels:join",
      bot_user_id: "U999",
      team: { id: "T123", name: "Locality" }
    },
    upstreamBody(input: RequestInfo | URL, init?: RequestInit) {
      return Object.fromEntries(new URLSearchParams((init as RequestInit).body as string));
    }
  }
] as const;
```

Add a helper after the existing session helpers:

```ts
async function startHostedSession(caseDef: (typeof hostedConnectorCases)[number]) {
  const hostedEnv = { ...env, ...caseDef.env } as BrokerEnv;
  const response = await app.request(caseDef.startPath, { method: "POST" }, hostedEnv);
  expect(response.status).toBe(200);
  return {
    hostedEnv,
    start: (await response.json()) as StartResponse
  };
}
```

Add this parameterized test block inside `describe("auth broker", ...)`:

```ts
describe.each(hostedConnectorCases)("$connector hosted handoff", (caseDef) => {
  it("starts OAuth with hosted provider callback and local loopback handoff", async () => {
    const { start } = await startHostedSession(caseDef);

    expect(start.connector).toBe(caseDef.connector);
    expect(start.redirect_uri).toBe(caseDef.localRedirectUri);
    expect(start.authorization_redirect_uri).toBe(caseDef.hostedCallbackUri);
    expect(start.exchange_redirect_uri).toBe(caseDef.hostedCallbackUri);
    const authorizationUrl = new URL(start.authorization_url);
    expect(authorizationUrl.searchParams.get("redirect_uri")).toBe(caseDef.hostedCallbackUri);
    expect(authorizationUrl.searchParams.get("state")).toBe(start.state);
    expect(start.session).toBeTruthy();
    expect(start.state).toBeTruthy();
    expect(start.session).not.toBe(start.state);
  });

  it("redirects a valid hosted callback to the local loopback listener", async () => {
    const { hostedEnv, start } = await startHostedSession(caseDef);
    const callback = await app.request(
      `${caseDef.callbackPath}?code=authorization-code&state=${encodeURIComponent(start.state)}`,
      { method: "GET" },
      hostedEnv
    );

    expect(callback.status).toBe(303);
    expect(callback.headers.get("cache-control")).toBe("no-store");
    expect(callback.headers.get("referrer-policy")).toBe("no-referrer");
    const location = new URL(callback.headers.get("location") ?? "");
    expect(`${location.origin}${location.pathname}`).toBe(caseDef.localRedirectUri);
    expect(location.searchParams.get("code")).toBe("authorization-code");
    expect(location.searchParams.get("state")).toBe(start.state);
  });

  it("redirects provider denial to the local loopback listener", async () => {
    const { hostedEnv, start } = await startHostedSession(caseDef);
    const callback = await app.request(
      `${caseDef.callbackPath}?error=access_denied&error_description=User%20cancelled&state=${encodeURIComponent(start.state)}`,
      { method: "GET" },
      hostedEnv
    );

    expect(callback.status).toBe(303);
    const location = new URL(callback.headers.get("location") ?? "");
    expect(`${location.origin}${location.pathname}`).toBe(caseDef.localRedirectUri);
    expect(location.searchParams.get("error")).toBe("access_denied");
    expect(location.searchParams.get("error_description")).toBe("User cancelled");
    expect(location.searchParams.get("state")).toBe(start.state);
    expect(location.searchParams.get("code")).toBeNull();
  });

  it("rejects hosted callback state that was not signed by the broker", async () => {
    const hostedEnv = { ...env, ...caseDef.env } as BrokerEnv;
    const callback = await app.request(
      `${caseDef.callbackPath}?code=authorization-code&state=not-signed`,
      { method: "GET" },
      hostedEnv
    );

    expect(callback.status).toBe(400);
    await expect(callback.json()).resolves.toMatchObject({
      error: { code: "invalid_state" }
    });
  });

  it("rejects hosted callback state with an unallowed local redirect URI", async () => {
    const hostedEnv = { ...env, ...caseDef.env } as BrokerEnv;
    const state = await signedLocalHandoffState({
      connector: caseDef.connector,
      local_redirect_uri: caseDef.localRedirectUri.replace("8757", "9999"),
      provider_redirect_uri: caseDef.hostedCallbackUri
    });

    const callback = await app.request(
      `${caseDef.callbackPath}?code=authorization-code&state=${encodeURIComponent(state)}`,
      { method: "GET" },
      hostedEnv
    );

    expect(callback.status).toBe(400);
    expect(callback.headers.get("location")).toBeNull();
    await expect(callback.json()).resolves.toMatchObject({
      error: { code: "redirect_uri_not_allowed" }
    });
  });

  it("rejects hosted callback state with the wrong provider redirect URI", async () => {
    const hostedEnv = { ...env, ...caseDef.env } as BrokerEnv;
    const state = await signedLocalHandoffState({
      connector: caseDef.connector,
      local_redirect_uri: caseDef.localRedirectUri,
      provider_redirect_uri: `${brokerOrigin}/v1/oauth/not-the-right-callback`
    });

    const callback = await app.request(
      `${caseDef.callbackPath}?code=authorization-code&state=${encodeURIComponent(state)}`,
      { method: "GET" },
      hostedEnv
    );

    expect(callback.status).toBe(400);
    expect(callback.headers.get("location")).toBeNull();
    await expect(callback.json()).resolves.toMatchObject({
      error: { code: "invalid_state" }
    });
  });

  it("exchanges authorization codes with the hosted redirect URI", async () => {
    const { hostedEnv, start } = await startHostedSession(caseDef);
    const fetchMock = vi.fn(async () => Response.json(caseDef.tokenResponse));
    globalThis.fetch = fetchMock as unknown as typeof fetch;

    const response = await app.request(
      caseDef.exchangePath,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          session: start.session,
          state: start.state,
          code: "authorization-code",
          redirect_uri: start.exchange_redirect_uri
        })
      },
      hostedEnv
    );

    expect(response.status).toBe(200);
    const upstreamBody = caseDef.upstreamBody(
      fetchMock.mock.calls[0]?.[0] as RequestInfo | URL,
      fetchMock.mock.calls[0]?.[1] as RequestInit
    );
    expect(upstreamBody).toMatchObject({
      grant_type: "authorization_code",
      code: "authorization-code",
      redirect_uri: caseDef.hostedCallbackUri
    });
  });

  it("rejects hosted exchange with an arbitrary redirect URI", async () => {
    const { hostedEnv, start } = await startHostedSession(caseDef);
    const fetchMock = vi.fn(async () => Response.json({ access_token: "unexpected" }));
    globalThis.fetch = fetchMock as unknown as typeof fetch;

    const response = await app.request(
      caseDef.exchangePath,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          session: start.session,
          state: start.state,
          code: "authorization-code",
          redirect_uri: "https://attacker.example.test/v1/oauth/callback"
        })
      },
      hostedEnv
    );

    expect(response.status).toBe(400);
    await expect(response.json()).resolves.toMatchObject({
      error: { code: "invalid_redirect_uri" }
    });
    expect(fetchMock).not.toHaveBeenCalled();
  });
});
```

Update `signedLocalHandoffState` to accept `connector`:

```ts
async function signedLocalHandoffState(
  overrides: Partial<{
    connector: "notion" | "google-docs" | "google-calendar" | "gmail" | "slack";
    local_redirect_uri: string;
    provider_redirect_uri: string;
  }>
) {
  return signLocalHandoffState(
    {
      v: 1,
      kind: "local_handoff",
      connector: overrides.connector ?? "notion",
      local_redirect_uri: overrides.local_redirect_uri ?? "http://localhost:8757/oauth/notion/callback",
      provider_redirect_uri: overrides.provider_redirect_uri ?? hostedNotionCallbackUri,
      iat: 1781179200,
      exp: 1781179800,
      nonce: "nonce"
    },
    env.LOCALITY_BROKER_SESSION_SECRET
  );
}
```

- [ ] **Step 2: Run the failing Worker test**

Run:

```bash
npm --prefix apps/oauth-service test -- app.test.ts
```

Expected: FAIL. Google Docs, Google Calendar, Gmail, and Slack hosted tests fail because those env fields, hosted callback routes, response fields, and exchange redirect validators do not exist yet.

- [ ] **Step 3: Add hosted callback env fields**

Modify `apps/oauth-service/src/types.ts`:

```ts
export interface BrokerEnv {
  LOCALITY_BROKER_SESSION_SECRET: string;
  LOCALITY_REFRESH_HANDLE_KEY?: string;
  LOCALITY_TOKEN_MODE?: "handle" | "raw";
  LOCALITY_NOTION_CLIENT_ID: string;
  LOCALITY_NOTION_CLIENT_SECRET: string;
  LOCALITY_NOTION_REDIRECT_URIS?: string;
  LOCALITY_NOTION_HOSTED_CALLBACK_URI?: string;
  LOCALITY_NOTION_AUTH_BASE_URL?: string;
  LOCALITY_NOTION_API_BASE_URL?: string;
  LOCALITY_NOTION_VERSION?: string;
  LOCALITY_GOOGLE_CLIENT_ID?: string;
  LOCALITY_GOOGLE_CLIENT_SECRET?: string;
  LOCALITY_GOOGLE_DOCS_REDIRECT_URIS?: string;
  LOCALITY_GOOGLE_DOCS_HOSTED_CALLBACK_URI?: string;
  LOCALITY_GOOGLE_DOCS_AUTH_BASE_URL?: string;
  LOCALITY_GOOGLE_DOCS_API_BASE_URL?: string;
  LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS?: string;
  LOCALITY_GOOGLE_CALENDAR_HOSTED_CALLBACK_URI?: string;
  LOCALITY_GOOGLE_CALENDAR_AUTH_BASE_URL?: string;
  LOCALITY_GOOGLE_CALENDAR_API_BASE_URL?: string;
  LOCALITY_GMAIL_REDIRECT_URIS?: string;
  LOCALITY_GMAIL_HOSTED_CALLBACK_URI?: string;
  LOCALITY_GMAIL_AUTH_BASE_URL?: string;
  LOCALITY_GMAIL_API_BASE_URL?: string;
  LOCALITY_SLACK_CLIENT_ID?: string;
  LOCALITY_SLACK_CLIENT_SECRET?: string;
  LOCALITY_SLACK_REDIRECT_URIS?: string;
  LOCALITY_SLACK_HOSTED_CALLBACK_URI?: string;
  LOCALITY_SLACK_AUTH_BASE_URL?: string;
  LOCALITY_SLACK_API_BASE_URL?: string;
}
```

- [ ] **Step 4: Generalize redirect validation**

Replace the Notion-only hosted helpers in `apps/oauth-service/src/security/redirects.ts` with connector-aware helpers:

```ts
interface ConnectorRedirectConfig {
  connector: "notion" | "google-docs" | "google-calendar" | "gmail" | "slack";
  displayName: string;
  hostedCallbackPath: string;
  allowedRedirectUris(env: BrokerEnv): string[];
  hostedCallbackValue(env: BrokerEnv): string | undefined;
}

const CONNECTOR_REDIRECT_CONFIGS: Record<ConnectorRedirectConfig["connector"], ConnectorRedirectConfig> = {
  notion: {
    connector: "notion",
    displayName: "Notion",
    hostedCallbackPath: "/v1/oauth/notion/callback",
    allowedRedirectUris: allowedNotionRedirectUris,
    hostedCallbackValue: (env) => env.LOCALITY_NOTION_HOSTED_CALLBACK_URI
  },
  "google-docs": {
    connector: "google-docs",
    displayName: "Google Docs",
    hostedCallbackPath: "/v1/oauth/google-docs/callback",
    allowedRedirectUris: allowedGoogleDocsRedirectUris,
    hostedCallbackValue: (env) => env.LOCALITY_GOOGLE_DOCS_HOSTED_CALLBACK_URI
  },
  "google-calendar": {
    connector: "google-calendar",
    displayName: "Google Calendar",
    hostedCallbackPath: "/v1/oauth/google-calendar/callback",
    allowedRedirectUris: allowedGoogleCalendarRedirectUris,
    hostedCallbackValue: (env) => env.LOCALITY_GOOGLE_CALENDAR_HOSTED_CALLBACK_URI
  },
  gmail: {
    connector: "gmail",
    displayName: "Gmail",
    hostedCallbackPath: "/v1/oauth/gmail/callback",
    allowedRedirectUris: allowedGmailRedirectUris,
    hostedCallbackValue: (env) => env.LOCALITY_GMAIL_HOSTED_CALLBACK_URI
  },
  slack: {
    connector: "slack",
    displayName: "Slack",
    hostedCallbackPath: "/v1/oauth/slack/callback",
    allowedRedirectUris: allowedSlackRedirectUris,
    hostedCallbackValue: (env) => env.LOCALITY_SLACK_HOSTED_CALLBACK_URI
  }
};

export function hostedConnectorCallbackUri(env: BrokerEnv, connector: ConnectorRedirectConfig["connector"]): string | undefined {
  const config = CONNECTOR_REDIRECT_CONFIGS[connector];
  const value = config.hostedCallbackValue(env)?.trim();
  if (!value) {
    return undefined;
  }
  return validateHostedConnectorCallbackUri(config, value);
}

export function validateHostedConnectorCallbackUri(config: ConnectorRedirectConfig, callbackUri: string): string {
  const hasExplicitPort = hasExplicitAuthorityPort(callbackUri);
  let parsed: URL;
  try {
    parsed = new URL(callbackUri);
  } catch {
    throw badRequest("invalid_hosted_callback_uri", `hosted ${config.displayName} callback URI must be a valid URL`);
  }
  if (
    parsed.protocol !== "https:" ||
    parsed.username !== "" ||
    parsed.password !== "" ||
    parsed.hostname === "" ||
    hasExplicitPort ||
    parsed.port !== "" ||
    parsed.pathname !== config.hostedCallbackPath ||
    parsed.search !== "" ||
    parsed.hash !== ""
  ) {
    throw badRequest(
      "invalid_hosted_callback_uri",
      `hosted ${config.displayName} callback URI must be an HTTPS URL at ${config.hostedCallbackPath} without userinfo, port, query, or fragment`
    );
  }
  return parsed.toString();
}

export function validateConnectorRedirectUri(env: BrokerEnv, connector: ConnectorRedirectConfig["connector"], redirectUri: string): string {
  const config = CONNECTOR_REDIRECT_CONFIGS[connector];
  return validateLoopbackRedirectUri(config.displayName, config.allowedRedirectUris(env), redirectUri);
}

export function validateConnectorExchangeRedirectUri(env: BrokerEnv, connector: ConnectorRedirectConfig["connector"], redirectUri: string): string {
  const hosted = hostedConnectorCallbackUri(env, connector);
  if (hosted && redirectUri === hosted) {
    return redirectUri;
  }
  return validateConnectorRedirectUri(env, connector, redirectUri);
}
```

Keep the existing connector-specific exports as wrappers:

```ts
export function hostedNotionCallbackUri(env: BrokerEnv): string | undefined {
  return hostedConnectorCallbackUri(env, "notion");
}

export function validateNotionExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "notion", redirectUri);
}

export function validateGoogleDocsExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "google-docs", redirectUri);
}

export function validateGoogleCalendarExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "google-calendar", redirectUri);
}

export function validateGmailExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "gmail", redirectUri);
}

export function validateSlackExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  return validateConnectorExchangeRedirectUri(env, "slack", redirectUri);
}
```

- [ ] **Step 5: Generalize Worker start/callback/exchange helpers**

In `apps/oauth-service/src/app.ts`, replace `NotionStartRedirects` with:

```ts
interface StartRedirects {
  localRedirectUri: string;
  authorizationRedirectUri: string;
  exchangeRedirectUri: string;
  hostedHandoff: boolean;
}

interface OAuthConnectorRuntime<TokenResponse> {
  connector: ConnectorId;
  defaultLocalRedirectUri: string;
  clientId(env: BrokerEnv): string | undefined;
  authorizeUrl(env: BrokerEnv, redirectUri: string, state: string): string;
  validateLocalRedirectUri(env: BrokerEnv, redirectUri: string): string;
  validateExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string;
  exchangeCode(env: BrokerEnv, code: string, redirectUri: string): Promise<TokenResponse>;
  shapeTokenResponse(env: BrokerEnv, token: TokenResponse): Promise<Record<string, unknown>>;
}
```

Add runtime configs below `const app = ...`:

```ts
const oauthConnectors = {
  notion: {
    connector: "notion",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/notion/callback",
    clientId: (env: BrokerEnv) => env.LOCALITY_NOTION_CLIENT_ID,
    authorizeUrl: notionAuthorizeUrl,
    validateLocalRedirectUri: validateNotionRedirectUri,
    validateExchangeRedirectUri: validateNotionExchangeRedirectUri,
    exchangeCode: exchangeNotionCode,
    shapeTokenResponse: shapeNotionTokenResponse
  },
  "google-docs": {
    connector: "google-docs",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/google-docs/callback",
    clientId: googleClientId,
    authorizeUrl: googleDocsAuthorizeUrl,
    validateLocalRedirectUri: validateGoogleDocsRedirectUri,
    validateExchangeRedirectUri: validateGoogleDocsExchangeRedirectUri,
    exchangeCode: exchangeGoogleDocsCode,
    shapeTokenResponse: shapeGoogleDocsTokenResponse
  },
  "google-calendar": {
    connector: "google-calendar",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/google-calendar/callback",
    clientId: googleClientId,
    authorizeUrl: googleCalendarAuthorizeUrl,
    validateLocalRedirectUri: validateGoogleCalendarRedirectUri,
    validateExchangeRedirectUri: validateGoogleCalendarExchangeRedirectUri,
    exchangeCode: exchangeGoogleCalendarCode,
    shapeTokenResponse: shapeGoogleCalendarTokenResponse
  },
  gmail: {
    connector: "gmail",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/gmail/callback",
    clientId: googleClientId,
    authorizeUrl: gmailAuthorizeUrl,
    validateLocalRedirectUri: validateGmailRedirectUri,
    validateExchangeRedirectUri: validateGmailExchangeRedirectUri,
    exchangeCode: exchangeGmailCode,
    shapeTokenResponse: shapeGmailTokenResponse
  },
  slack: {
    connector: "slack",
    defaultLocalRedirectUri: "http://localhost:8757/oauth/slack/callback",
    clientId: (env: BrokerEnv) => env.LOCALITY_SLACK_CLIENT_ID,
    authorizeUrl: slackAuthorizeUrl,
    validateLocalRedirectUri: validateSlackRedirectUri,
    validateExchangeRedirectUri: validateSlackExchangeRedirectUri,
    exchangeCode: exchangeSlackCode,
    shapeTokenResponse: shapeSlackTokenResponse
  }
} satisfies Record<ConnectorId, OAuthConnectorRuntime<unknown>>;
```

Add helpers near `requiredJson`:

```ts
async function startOAuthConnector<TokenResponse>(
  env: BrokerEnv,
  connector: OAuthConnectorRuntime<TokenResponse>,
  body: StartRequest
) {
  const redirects = startRedirects(env, connector.connector, connector.validateLocalRedirectUri, body.redirect_uri ?? connector.defaultLocalRedirectUri);
  const now = nowSeconds();
  const secret = requireOperationalSecret(env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET");
  const state = redirects.hostedHandoff
    ? await signLocalHandoffState(
        {
          v: 1,
          kind: "local_handoff",
          connector: connector.connector,
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
      connector: connector.connector,
      state,
      redirect_uri: redirects.exchangeRedirectUri,
      iat: now,
      exp: now + SESSION_TTL_SECONDS,
      nonce: randomBase64Url()
    },
    secret
  );
  return {
    connector: connector.connector,
    client_id: connector.clientId(env),
    authorization_url: connector.authorizeUrl(env, redirects.authorizationRedirectUri, state),
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

async function exchangeOAuthConnector<TokenResponse>(
  env: BrokerEnv,
  connector: OAuthConnectorRuntime<TokenResponse>,
  body: ExchangeRequest
) {
  const session = requireString(body.session, "session");
  const state = requireString(body.state, "state");
  const code = requireString(body.code, "code");
  const redirectUri = connector.validateExchangeRedirectUri(env, requireString(body.redirect_uri, "redirect_uri"));
  const payload = await verifySession(
    session,
    requireOperationalSecret(env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  if (payload.connector !== connector.connector || payload.state !== state || payload.redirect_uri !== redirectUri) {
    throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
  }
  const token = await connector.exchangeCode(env, code, redirectUri);
  return connector.shapeTokenResponse(env, token);
}

function startRedirects(
  env: BrokerEnv,
  connector: ConnectorId,
  validateLocalRedirectUri: (env: BrokerEnv, redirectUri: string) => string,
  requestedRedirectUri: string
): StartRedirects {
  const localRedirectUri = validateLocalRedirectUri(env, requestedRedirectUri);
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
```

Replace every connector start route with the helper, for example:

```ts
app.post("/v1/oauth/google-docs/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  return c.json(await startOAuthConnector(c.env, oauthConnectors["google-docs"], body));
});
```

Add hosted callback routes for Google Docs, Google Calendar, Gmail, and Slack:

```ts
app.get("/v1/oauth/google-docs/callback", async (c) =>
  hostedCallbackResponse(c.env, "google-docs", validateGoogleDocsRedirectUri, c.req.query() as HostedCallbackQuery)
);

app.get("/v1/oauth/google-calendar/callback", async (c) =>
  hostedCallbackResponse(c.env, "google-calendar", validateGoogleCalendarRedirectUri, c.req.query() as HostedCallbackQuery)
);

app.get("/v1/oauth/gmail/callback", async (c) =>
  hostedCallbackResponse(c.env, "gmail", validateGmailRedirectUri, c.req.query() as HostedCallbackQuery)
);

app.get("/v1/oauth/slack/callback", async (c) =>
  hostedCallbackResponse(c.env, "slack", validateSlackRedirectUri, c.req.query() as HostedCallbackQuery)
);
```

Replace every connector exchange route with the helper, for example:

```ts
app.post("/v1/oauth/slack/exchange", async (c) => {
  const body = await requiredJson<ExchangeRequest>(c.req.raw);
  return c.json(await exchangeOAuthConnector(c.env, oauthConnectors.slack, body));
});
```

- [ ] **Step 6: Run Worker tests and typecheck**

Run:

```bash
npm --prefix apps/oauth-service test -- app.test.ts
npm --prefix apps/oauth-service run typecheck
```

Expected: PASS. The hosted parameterized tests pass for all five connectors, and TypeScript compiles.

- [ ] **Step 7: Run the full OAuth service check**

Run:

```bash
npm --prefix apps/oauth-service run check
```

Expected: PASS.

- [ ] **Step 8: Commit Worker implementation**

Run:

```bash
git add apps/oauth-service/src/types.ts apps/oauth-service/src/security/redirects.ts apps/oauth-service/src/app.ts apps/oauth-service/test/app.test.ts
git commit -m "feat(oauth): host callback handoff for all connectors"
```

Expected: commit succeeds.

---

### Task 2: Teach Shared Rust Broker Responses About Hosted Redirects

**Files:**
- Modify: `crates/locality-connector/src/oauth_broker.rs`
- Modify: `crates/loc-cli/src/commands.rs`
- Modify: `apps/desktop/src-tauri/src/main.rs`
- Modify: `crates/loc-cli/tests/connect.rs`

- [ ] **Step 1: Write failing shared response tests**

In `crates/locality-connector/src/oauth_broker.rs`, update the test module import:

```rust
use super::{OAuthBrokerStart, OAuthBrokerStartResponse, OAuthBrokerToken};
```

Add tests in the test module:

```rust
#[test]
fn start_response_defaults_hosted_redirects_to_local_redirect() {
    let payload = serde_json::json!({
        "connector": "google-docs",
        "client_id": "google-client-id",
        "authorization_url": "https://accounts.google.com/o/oauth2/v2/auth?client_id=google-client-id",
        "redirect_uri": "http://localhost:8757/oauth/google-docs/callback",
        "session": "session-1",
        "state": "state-1",
        "expires_in": 600
    });

    let response: OAuthBrokerStartResponse =
        serde_json::from_value(payload).expect("decode start response");

    assert_eq!(
        response.local_redirect_uri(),
        "http://localhost:8757/oauth/google-docs/callback"
    );
    assert_eq!(
        response.authorization_redirect_uri(),
        "http://localhost:8757/oauth/google-docs/callback"
    );
    assert_eq!(
        response.exchange_redirect_uri(),
        "http://localhost:8757/oauth/google-docs/callback"
    );
}

#[test]
fn start_response_uses_hosted_redirects_when_present() {
    let response = OAuthBrokerStartResponse {
        connector: "gmail".to_string(),
        client_id: "google-client-id".to_string(),
        authorization_url: "https://accounts.google.com/o/oauth2/v2/auth?client_id=google-client-id".to_string(),
        redirect_uri: "http://localhost:8757/oauth/gmail/callback".to_string(),
        authorization_redirect_uri: Some(
            "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback".to_string(),
        ),
        exchange_redirect_uri: Some(
            "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback".to_string(),
        ),
        session: "session-1".to_string(),
        state: "state-1".to_string(),
        expires_in: 600,
    };

    assert_eq!(
        response.local_redirect_uri(),
        "http://localhost:8757/oauth/gmail/callback"
    );
    assert_eq!(
        response.authorization_redirect_uri(),
        "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback"
    );
    assert_eq!(
        response.exchange_redirect_uri(),
        "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback"
    );
}
```

- [ ] **Step 2: Run the failing shared response tests**

Run:

```bash
cargo test -p locality-connector start_response_
```

Expected: FAIL to compile because `OAuthBrokerStartResponse` has no hosted redirect fields or helper methods.

- [ ] **Step 3: Implement shared response fields and helpers**

Modify `OAuthBrokerStartResponse` in `crates/locality-connector/src/oauth_broker.rs`:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuthBrokerStartResponse {
    pub connector: String,
    pub client_id: String,
    pub authorization_url: String,
    pub redirect_uri: String,
    #[serde(default)]
    pub authorization_redirect_uri: Option<String>,
    #[serde(default)]
    pub exchange_redirect_uri: Option<String>,
    pub session: String,
    pub state: String,
    pub expires_in: u64,
}

impl OAuthBrokerStartResponse {
    pub fn local_redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub fn authorization_redirect_uri(&self) -> &str {
        self.authorization_redirect_uri
            .as_deref()
            .unwrap_or(&self.redirect_uri)
    }

    pub fn exchange_redirect_uri(&self) -> &str {
        self.exchange_redirect_uri
            .as_deref()
            .unwrap_or(&self.redirect_uri)
    }
}
```

- [ ] **Step 4: Update CLI command flows**

In `crates/loc-cli/src/commands.rs`, update Google Docs, Google Calendar, Gmail, and Slack broker flows. Each flow should listen on `start.local_redirect_uri()` and exchange with `start.exchange_redirect_uri()`.

For Google Docs, replace the listener and options construction with:

```rust
let authorization = match run_local_oauth_authorization(
    "Google Docs",
    &start.authorization_url,
    start.local_redirect_uri(),
    &start.state,
    has_flag(args, "--no-browser"),
    json,
) {
    Ok(authorization) => authorization,
    Err(error) => {
        return command_error(
            json,
            google_docs_local_oauth_command_error(error),
            EXIT_INTERNAL,
        );
    }
};
let exchange_redirect_uri = start.exchange_redirect_uri().to_string();
let options = GoogleDocsBrokerOAuthConnectOptions {
    connection_id: flag_value(args, "--name").map(ConnectionId::new),
    broker_url: broker_config.broker_url,
    client_id: start.client_id,
    session: start.session,
    state: start.state,
    code: authorization.code,
    redirect_uri: exchange_redirect_uri,
};
```

Apply the same exact pattern to Google Calendar, Gmail, and Slack with their provider names and option types.

- [ ] **Step 5: Update desktop broker flows**

In `apps/desktop/src-tauri/src/main.rs`, update `connect_google_docs_with_broker`, `connect_google_calendar_with_broker`, `connect_gmail_with_broker`, and `connect_slack_with_broker`.

For Gmail, the final shape should be:

```rust
let authorization = run_local_oauth_authorization(
    "Gmail",
    &start.authorization_url,
    start.local_redirect_uri(),
    &start.state,
    !open_browser,
    true,
)
.map_err(|error| error.message)?;
let exchange_redirect_uri = start.exchange_redirect_uri().to_string();
let options = GmailBrokerOAuthConnectOptions {
    connection_id: None,
    broker_url,
    client_id: start.client_id,
    session: start.session,
    state: start.state,
    code: authorization.code,
    redirect_uri: exchange_redirect_uri,
};
```

Apply the same exact pattern to Google Docs, Google Calendar, and Slack with their provider names and option types.

- [ ] **Step 6: Add hosted exchange redirect tests for non-Notion connectors**

In `crates/loc-cli/tests/connect.rs`, make the fake exchange structs configurable:

```rust
#[derive(Clone, Debug)]
struct FakeGoogleDocsBrokerOAuthExchange {
    expected_redirect_uri: &'static str,
}

impl Default for FakeGoogleDocsBrokerOAuthExchange {
    fn default() -> Self {
        Self {
            expected_redirect_uri: "http://localhost:8757/oauth/google-docs/callback",
        }
    }
}
```

Update its assertion:

```rust
assert_eq!(request.redirect_uri, self.expected_redirect_uri);
```

Apply the same pattern to:

- `FakeGmailBrokerOAuthExchange`
- `FakeGoogleCalendarBrokerOAuthExchange`
- `FakeSlackBrokerOAuthExchange`

Update existing test setup from unit structs to defaults, for example:

```rust
let exchange = FakeGoogleDocsBrokerOAuthExchange::default();
```

Add hosted storage tests:

```rust
#[test]
fn connect_google_docs_broker_oauth_can_store_local_credentials_after_hosted_exchange_redirect() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();

    let report = run_connect_google_docs_broker_oauth(
        &mut store,
        &credentials,
        GoogleDocsBrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("docs-hosted")),
            broker_url: "https://afs-oauth-broker.saurabh-b07.workers.dev".to_string(),
            client_id: "google-client-id".to_string(),
            session: "broker-session".to_string(),
            state: "state-1".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-docs/callback".to_string(),
        },
        &FakeGoogleDocsBrokerOAuthExchange {
            expected_redirect_uri: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-docs/callback",
        },
    )
    .expect("broker OAuth connect");

    assert_eq!(report.connection_id, "docs-hosted");
    assert_eq!(report.auth_kind, "oauth");
    let saved = store
        .get_connection(&ConnectionId::new("docs-hosted"))
        .expect("get connection")
        .expect("saved connection");
    assert_eq!(saved.auth_kind, "oauth");
    assert_eq!(saved.secret_ref, "connection:docs-hosted");
    let secret = credentials
        .get("connection:docs-hosted")
        .expect("credential saved");
    assert!(secret.contains("\"oauth_broker_url\":\"https://afs-oauth-broker.saurabh-b07.workers.dev\""));
    assert!(secret.contains("\"refresh_token_handle\":\"opaque-refresh-handle\""));
}
```

Add equivalent tests for:

- `connect_google_calendar_broker_oauth_can_store_local_credentials_after_hosted_exchange_redirect` with connection ID `google-calendar-hosted` and callback `/v1/oauth/google-calendar/callback`
- `connect_gmail_broker_oauth_can_store_local_credentials_after_hosted_exchange_redirect` with connection ID `gmail-hosted` and callback `/v1/oauth/gmail/callback`
- `connect_slack_broker_oauth_can_store_local_credentials_after_hosted_exchange_redirect` with connection ID `slack-hosted` and callback `/v1/oauth/slack/callback`

- [ ] **Step 7: Run focused Rust tests**

Run:

```bash
cargo test -p locality-connector start_response_
cargo test -p loc-cli hosted_exchange_redirect
```

Expected: PASS. If the second filter does not match because test names include more text, run:

```bash
cargo test -p loc-cli can_store_local_credentials_after_hosted_exchange_redirect
```

Expected: PASS.

- [ ] **Step 8: Run broader Rust checks**

Run:

```bash
cargo test -p locality-connector oauth_broker::tests
cargo test -p loc-cli connect
cargo check -p locality-desktop
```

Expected: PASS. Existing warnings in unrelated crates are acceptable if the commands exit 0.

- [ ] **Step 9: Commit Rust implementation**

Run:

```bash
git add crates/locality-connector/src/oauth_broker.rs crates/loc-cli/src/commands.rs apps/desktop/src-tauri/src/main.rs crates/loc-cli/tests/connect.rs
git commit -m "feat(cli): use hosted callback redirects for all broker oauth"
```

Expected: commit succeeds.

---

### Task 3: Update Config And Documentation For All Hosted Callback Paths

**Files:**
- Modify: `apps/oauth-service/wrangler.toml`
- Modify: `apps/oauth-service/README.md`
- Modify: `apps/oauth-service/docs/deployment.md`
- Modify: `apps/oauth-service/docs/security.md`
- Modify: `docs/cli.md`
- Modify: `docs-site/cli-reference.mdx`
- Modify: `docs/notion-connector.md`

- [ ] **Step 1: Update Worker config**

Modify `[vars]` in `apps/oauth-service/wrangler.toml` to include every local allowlist and hosted callback:

```toml
LOCALITY_TOKEN_MODE = "handle"
LOCALITY_NOTION_REDIRECT_URIS = "http://localhost:8757/oauth/notion/callback,http://127.0.0.1:8757/oauth/notion/callback"
LOCALITY_NOTION_HOSTED_CALLBACK_URI = "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
LOCALITY_GOOGLE_DOCS_REDIRECT_URIS = "http://localhost:8757/oauth/google-docs/callback,http://127.0.0.1:8757/oauth/google-docs/callback"
LOCALITY_GOOGLE_DOCS_HOSTED_CALLBACK_URI = "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-docs/callback"
LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS = "http://localhost:8757/oauth/google-calendar/callback,http://127.0.0.1:8757/oauth/google-calendar/callback"
LOCALITY_GOOGLE_CALENDAR_HOSTED_CALLBACK_URI = "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-calendar/callback"
LOCALITY_GMAIL_REDIRECT_URIS = "http://localhost:8757/oauth/gmail/callback,http://127.0.0.1:8757/oauth/gmail/callback"
LOCALITY_GMAIL_HOSTED_CALLBACK_URI = "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback"
LOCALITY_SLACK_REDIRECT_URIS = "http://localhost:8757/oauth/slack/callback,http://127.0.0.1:8757/oauth/slack/callback"
LOCALITY_SLACK_HOSTED_CALLBACK_URI = "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/slack/callback"
```

- [ ] **Step 2: Update OAuth service README**

In `apps/oauth-service/README.md`, replace Notion-only hosted wording with connector-generic wording:

```md
When `LOCALITY_<CONNECTOR>_HOSTED_CALLBACK_URI` is set, `redirect_uri` remains
the local loopback URI where the client listens. `authorization_redirect_uri`
and `exchange_redirect_uri` are the HTTPS provider callback URI registered with
the provider. The browser first returns to the broker callback, and the broker
redirects the browser to `redirect_uri` with the provider code or error. The
client then exchanges the code using `exchange_redirect_uri`, so the provider
sees the same redirect URI during authorization and token exchange.
```

Add a callback section covering all connector paths:

```md
### `GET /v1/oauth/<connector>/callback`

Hosted callback routes exist for:

- `/v1/oauth/notion/callback`
- `/v1/oauth/google-docs/callback`
- `/v1/oauth/google-calendar/callback`
- `/v1/oauth/gmail/callback`
- `/v1/oauth/slack/callback`

These browser-facing routes are used only when the corresponding
`LOCALITY_<CONNECTOR>_HOSTED_CALLBACK_URI` is configured. A route accepts the
provider's `code` and `state`, verifies the signed local-handoff state, and
returns `303 See Other` to the loopback callback held inside that state.
```

Update Google Docs, Google Calendar, Gmail, and Slack start/exchange examples to show `authorization_redirect_uri`, `exchange_redirect_uri`, and hosted exchange redirect behavior, matching the Notion examples.

- [ ] **Step 3: Update security and deployment docs**

In `apps/oauth-service/docs/security.md`, replace the Redirects section with:

```md
The broker keeps two redirect boundaries separate for every OAuth connector:

- `LOCALITY_<CONNECTOR>_REDIRECT_URIS` is a loopback-only allowlist for local callbacks such as `http://localhost:8757/oauth/gmail/callback`.
- `LOCALITY_<CONNECTOR>_HOSTED_CALLBACK_URI` is one exact HTTPS callback served by this broker at the connector's `/v1/oauth/<connector>/callback` path.

When hosted handoff is enabled, the provider authorization request uses the
hosted callback URI. The callback route verifies a signed state payload before
redirecting to a loopback URI from the allowlist. The token exchange also uses
the hosted callback URI so the provider sees the same redirect URI in both OAuth
steps.
```

In `apps/oauth-service/docs/deployment.md`, document provider registration:

~~~md
Register these hosted callback URIs before deploying the matching
`LOCALITY_<CONNECTOR>_HOSTED_CALLBACK_URI` values:

```text
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-docs/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-calendar/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/slack/callback
```

The Google OAuth client must include the Google Docs, Google Calendar, and Gmail
hosted callback URIs. Slack must include the Slack hosted callback URI. Notion
must include the Notion hosted callback URI.
~~~

- [ ] **Step 4: Update CLI docs**

In `docs/cli.md` and `docs-site/cli-reference.mdx`, update the Google Docs, Google Calendar, Gmail, and Slack OAuth paragraphs to mirror the Notion statement:

```md
The default local callback is `http://localhost:8757/oauth/<connector>/callback`; override it with `--redirect-uri <uri>` or the connector-specific `LOCALITY_<CONNECTOR>_OAUTH_REDIRECT_URI`. In production the broker may use its own HTTPS provider callback registered on the provider app, then hand the browser back to the local callback. The command still stores the resulting OAuth credential locally.
```

Keep direct OAuth or provider-specific scope details unchanged.

- [ ] **Step 5: Update Notion connector doc**

In `docs/notion-connector.md`, replace the final Notion-only hosted wording with:

```md
The CLI still listens on the local callback, defaulting to
`http://localhost:8757/oauth/notion/callback`. In production the Notion public
integration should register the broker's HTTPS hosted callback; the broker
receives that callback and redirects the browser back to the local listener.
For BYO/direct OAuth development, a developer-owned Notion app may still
register and use the local callback.
```

- [ ] **Step 6: Run docs and service checks**

Run:

```bash
npm --prefix apps/oauth-service run check
make docs-validate
make docs-broken-links
```

Expected: PASS.

- [ ] **Step 7: Commit docs and config**

Run:

```bash
git add apps/oauth-service/wrangler.toml apps/oauth-service/README.md apps/oauth-service/docs/deployment.md apps/oauth-service/docs/security.md docs/cli.md docs-site/cli-reference.mdx docs/notion-connector.md
git commit -m "docs(oauth): document hosted callbacks for all connectors"
```

Expected: commit succeeds.

---

### Task 4: Final Verification And PR Update

**Files:**
- No source edits expected unless smoke tests reveal a concrete mismatch.

- [ ] **Step 1: Run full affected verification**

Run:

```bash
npm --prefix apps/oauth-service run check
cargo test -p locality-connector oauth_broker::tests
cargo test -p locality-notion oauth::tests
cargo test -p loc-cli connect
cargo check -p locality-desktop
make check-oauth-service
make docs-validate
make docs-broken-links
```

Expected: every command exits 0. Existing unrelated Rust warnings are acceptable.

- [ ] **Step 2: Run a local Worker smoke test for Google Docs and Slack**

Prepare local Worker variables. Preserve any existing local `.dev.vars`:

```bash
if [ -f apps/oauth-service/.dev.vars ]; then
  cp apps/oauth-service/.dev.vars apps/oauth-service/.dev.vars.before-all-hosted-handoff-smoke
fi
cat > apps/oauth-service/.dev.vars <<'EOF'
LOCALITY_BROKER_SESSION_SECRET=test-session-secret-with-enough-entropy
LOCALITY_REFRESH_HANDLE_KEY=test-refresh-handle-key-with-enough-entropy
LOCALITY_TOKEN_MODE=handle
LOCALITY_NOTION_CLIENT_ID=notion-client-id
LOCALITY_NOTION_CLIENT_SECRET=notion-client-secret
LOCALITY_NOTION_REDIRECT_URIS=http://localhost:8757/oauth/notion/callback,http://127.0.0.1:8757/oauth/notion/callback
LOCALITY_NOTION_HOSTED_CALLBACK_URI=https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback
LOCALITY_GOOGLE_CLIENT_ID=google-client-id
LOCALITY_GOOGLE_CLIENT_SECRET=google-client-secret
LOCALITY_GOOGLE_DOCS_REDIRECT_URIS=http://localhost:8757/oauth/google-docs/callback,http://127.0.0.1:8757/oauth/google-docs/callback
LOCALITY_GOOGLE_DOCS_HOSTED_CALLBACK_URI=https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-docs/callback
LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS=http://localhost:8757/oauth/google-calendar/callback,http://127.0.0.1:8757/oauth/google-calendar/callback
LOCALITY_GOOGLE_CALENDAR_HOSTED_CALLBACK_URI=https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-calendar/callback
LOCALITY_GMAIL_REDIRECT_URIS=http://localhost:8757/oauth/gmail/callback,http://127.0.0.1:8757/oauth/gmail/callback
LOCALITY_GMAIL_HOSTED_CALLBACK_URI=https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback
LOCALITY_SLACK_CLIENT_ID=slack-client-id
LOCALITY_SLACK_CLIENT_SECRET=slack-client-secret
LOCALITY_SLACK_REDIRECT_URIS=http://localhost:8757/oauth/slack/callback,http://127.0.0.1:8757/oauth/slack/callback
LOCALITY_SLACK_HOSTED_CALLBACK_URI=https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/slack/callback
EOF
```

Start the Worker:

```bash
npm --prefix apps/oauth-service run dev
```

In another shell, validate Google Docs start:

```bash
node <<'EOF'
const expectedHosted = 'https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-docs/callback';
const expectedLocal = 'http://localhost:8757/oauth/google-docs/callback';
const response = await fetch('http://127.0.0.1:8787/v1/oauth/google-docs/start', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ redirect_uri: expectedLocal })
});
if (!response.ok) throw new Error(`google-docs start failed: ${response.status} ${await response.text()}`);
const body = await response.json();
const authorizationUrl = new URL(body.authorization_url);
if (body.redirect_uri !== expectedLocal) throw new Error('local redirect mismatch');
if (body.authorization_redirect_uri !== expectedHosted) throw new Error('authorization redirect mismatch');
if (body.exchange_redirect_uri !== expectedHosted) throw new Error('exchange redirect mismatch');
if (authorizationUrl.searchParams.get('redirect_uri') !== expectedHosted) throw new Error('authorization URL redirect mismatch');
if (authorizationUrl.searchParams.get('state') !== body.state) throw new Error('state mismatch');
console.log('google-docs hosted start ok');
EOF
```

Validate Slack start:

```bash
node <<'EOF'
const expectedHosted = 'https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/slack/callback';
const expectedLocal = 'http://localhost:8757/oauth/slack/callback';
const response = await fetch('http://127.0.0.1:8787/v1/oauth/slack/start', {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ redirect_uri: expectedLocal })
});
if (!response.ok) throw new Error(`slack start failed: ${response.status} ${await response.text()}`);
const body = await response.json();
const authorizationUrl = new URL(body.authorization_url);
if (body.redirect_uri !== expectedLocal) throw new Error('local redirect mismatch');
if (body.authorization_redirect_uri !== expectedHosted) throw new Error('authorization redirect mismatch');
if (body.exchange_redirect_uri !== expectedHosted) throw new Error('exchange redirect mismatch');
if (authorizationUrl.searchParams.get('redirect_uri') !== expectedHosted) throw new Error('authorization URL redirect mismatch');
if (authorizationUrl.searchParams.get('state') !== body.state) throw new Error('state mismatch');
console.log('slack hosted start ok');
EOF
```

Stop the Worker and restore local vars:

```bash
rm apps/oauth-service/.dev.vars
if [ -f apps/oauth-service/.dev.vars.before-all-hosted-handoff-smoke ]; then
  mv apps/oauth-service/.dev.vars.before-all-hosted-handoff-smoke apps/oauth-service/.dev.vars
fi
```

Expected: both smoke scripts print success and no local `.dev.vars` changes remain in `git status --short`.

- [ ] **Step 3: Run a CLI no-browser smoke test for Google Docs**

With the Worker running from Step 2, run:

```bash
LOCALITY_GOOGLE_DOCS_OAUTH_BROKER_URL=http://127.0.0.1:8787 \
  cargo run -p loc-cli -- connect google-docs --name hosted-docs-smoke --no-browser
```

Expected:

- The command prints a local callback at `http://localhost:8757/oauth/google-docs/callback`.
- The authorization URL contains `redirect_uri=https%3A%2F%2Fafs-oauth-broker.saurabh-b07.workers.dev%2Fv1%2Foauth%2Fgoogle-docs%2Fcallback`.
- Stop the command with Ctrl-C after verifying the URL and listener output.

- [ ] **Step 4: Confirm external provider registration gate**

Before deploying the Worker vars, verify in provider dashboards that the production apps include exactly these hosted callbacks:

```text
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-docs/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-calendar/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/slack/callback
```

Expected: This is a manual external gate. If not verified, report it as pending and do not claim deployment readiness.

- [ ] **Step 5: Update the PR**

Run:

```bash
git status --short
git push
gh pr edit 132 --title "Support hosted OAuth callback handoff for all connectors" --body "$(cat <<'EOF'
## Summary
- Add hosted OAuth callback handoff for Notion, Google Docs, Google Calendar, Gmail, and Slack while keeping local loopback listeners and local credential storage.
- Teach CLI and desktop broker flows to listen on local `redirect_uri` and exchange with hosted `exchange_redirect_uri` when returned by the broker.
- Update Worker config and docs so loopback allowlists and hosted provider callbacks are separate for every OAuth connector.

## Test Plan
- [x] npm --prefix apps/oauth-service run check
- [x] cargo test -p locality-connector oauth_broker::tests
- [x] cargo test -p locality-notion oauth::tests
- [x] cargo test -p loc-cli connect
- [x] cargo check -p locality-desktop
- [x] make check-oauth-service
- [x] make docs-validate
- [x] make docs-broken-links
- [x] Local Worker smoke for Google Docs and Slack hosted start responses
- [x] CLI no-browser smoke for Google Docs hosted redirect URL

## Deployment Gate
Before deploying/enabling hosted callbacks, register the per-connector HTTPS callback URLs in the provider apps. The Google OAuth app must include the Google Docs, Google Calendar, and Gmail callback paths.
EOF
)"
```

Expected: branch pushes and PR title/body are updated.

---

## Self-Review

Spec coverage:

- All OAuth connectors use hosted callback paths: Task 1 and Task 3.
- Per-connector callback paths: Task 1 tests and config/docs in Task 3.
- Local credentials and local listener remain: Task 2 CLI/desktop changes and Task 4 smoke.
- Provider exchange uses same hosted redirect URI as authorization: Task 1 exchange tests and Task 2 shared response helpers.
- No private backend changes: no private backend files are in this plan.

Placeholder scan:

- The plan contains no unresolved markers or copied-reference implementation instructions.
- Repeated connector work is listed with exact connector names, paths, and expected callback URIs.

Type consistency:

- JSON fields are consistently `authorization_redirect_uri` and `exchange_redirect_uri`.
- Rust methods are consistently `local_redirect_uri()`, `authorization_redirect_uri()`, and `exchange_redirect_uri()`.
- Worker env fields are consistently `LOCALITY_<CONNECTOR>_HOSTED_CALLBACK_URI`.
