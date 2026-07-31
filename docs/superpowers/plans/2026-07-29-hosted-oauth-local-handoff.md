# Hosted OAuth Local Handoff Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let Notion redirect to the deployed TLS OAuth service while preserving today's local `loc connect notion` outcome: the CLI receives the authorization code on localhost, exchanges through the broker, and stores local credentials in the OS credential store.

**Architecture:** The OAuth broker remains the confidential token exchange boundary. For Notion only, `/v1/oauth/notion/start` keeps accepting a local loopback `redirect_uri`, but when `LOCALITY_NOTION_HOSTED_CALLBACK_URI` is configured it sends Notion an HTTPS broker callback URI and returns separate local, authorization, and exchange redirect URIs. The broker callback verifies a signed local-handoff state token and redirects the browser to the local loopback listener with the original code or provider error.

**Tech Stack:** Cloudflare Worker, Hono, TypeScript, Vitest, Rust, Serde, reqwest, existing `loc` CLI OAuth flow.

---

## Scope

This plan covers Notion first. Google Docs, Gmail, Google Calendar, and Slack stay on the existing loopback redirect flow because their client/broker structs and provider registration needs are separate.

The final behavior is:

1. `loc connect notion` starts a local listener at `http://localhost:8757/oauth/notion/callback`.
2. The CLI calls the broker start endpoint with that local URI.
3. The broker returns an authorization URL whose `redirect_uri` is the TLS broker callback.
4. Notion redirects the browser to the TLS broker callback.
5. The broker validates signed state and redirects the browser to localhost with `code` and `state`.
6. The CLI validates `state`, receives `code`, and calls broker exchange with the hosted HTTPS exchange redirect URI.
7. The broker exchanges the code with Notion using the same hosted HTTPS redirect URI and returns the token response.
8. The CLI stores the credential locally exactly as it does today.

## File Structure

- Modify `apps/oauth-service/src/types.ts`: add the Notion hosted callback environment variable.
- Modify `apps/oauth-service/src/security/session.ts`: add signed local-handoff state helpers.
- Modify `apps/oauth-service/src/security/redirects.ts`: keep `LOCALITY_NOTION_REDIRECT_URIS` loopback-only and add exact HTTPS callback validation.
- Modify `apps/oauth-service/src/app.ts`: add hosted Notion start behavior, the `GET /v1/oauth/notion/callback` route, and exchange redirect validation.
- Modify `apps/oauth-service/test/app.test.ts`: add broker behavior tests for hosted handoff.
- Create `apps/oauth-service/test/local-handoff-state.test.ts`: focused tests for signed state helpers.
- Modify `apps/oauth-service/README.md`: document the hosted handoff flow and API response fields.
- Modify `apps/oauth-service/docs/security.md`: document redirect boundaries and non-persistence of callback codes.
- Modify `apps/oauth-service/wrangler.toml`: split loopback allowlist from hosted provider callback config.
- Modify `crates/locality-notion/src/oauth.rs`: extend `NotionOAuthBrokerStartResponse` with optional broker-provided authorization and exchange redirect URIs.
- Modify `crates/loc-cli/src/commands.rs`: listen on the local URI while exchanging with the broker-provided exchange URI.
- Modify `docs/cli.md` and `docs-site/cli-reference.mdx`: document that the default broker may use a hosted provider callback while the CLI still listens locally.

---

### Task 1: Add Signed Local-Handoff State Helpers

**Files:**
- Modify: `apps/oauth-service/src/security/session.ts`
- Create: `apps/oauth-service/test/local-handoff-state.test.ts`

- [ ] **Step 1: Write the failing signed-state tests**

Create `apps/oauth-service/test/local-handoff-state.test.ts`:

```ts
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
```

- [ ] **Step 2: Run the failing tests**

Run:

```bash
npm --prefix apps/oauth-service test -- local-handoff-state.test.ts
```

Expected: FAIL with TypeScript or runtime errors stating `signLocalHandoffState` and `verifyLocalHandoffState` are not exported.

- [ ] **Step 3: Add the signed-state helpers**

Modify `apps/oauth-service/src/security/session.ts` so the top-level exported types and functions include the following code. Keep the existing `OAuthSessionPayload`, `signSession`, `verifySession`, and `nowSeconds` exports.

```ts
import { badRequest, unauthorized } from "../http/errors";
import type { ConnectorId } from "../types";
import { constantTimeEqual, hmacSha256Base64Url, parseUtf8Base64Url, utf8Base64Url } from "./crypto";

export interface OAuthSessionPayload {
  v: 1;
  connector: ConnectorId;
  state: string;
  redirect_uri: string;
  iat: number;
  exp: number;
  nonce: string;
}

export interface OAuthLocalHandoffStatePayload {
  v: 1;
  kind: "local_handoff";
  connector: ConnectorId;
  local_redirect_uri: string;
  provider_redirect_uri: string;
  iat: number;
  exp: number;
  nonce: string;
}

export async function signSession(payload: OAuthSessionPayload, secret: string): Promise<string> {
  return signPayload(payload, secret);
}

export async function verifySession(token: string, secret: string, now = nowSeconds()): Promise<OAuthSessionPayload> {
  const payload = await verifyPayload<OAuthSessionPayload>(token, secret, "session");
  if (!isOAuthSessionPayload(payload)) {
    throw badRequest("invalid_session", "OAuth session token payload is invalid");
  }
  if (payload.exp <= now) {
    throw unauthorized("expired_session", "OAuth session has expired");
  }
  return payload;
}

export async function signLocalHandoffState(payload: OAuthLocalHandoffStatePayload, secret: string): Promise<string> {
  return signPayload(payload, secret);
}

export async function verifyLocalHandoffState(
  token: string,
  secret: string,
  now = nowSeconds()
): Promise<OAuthLocalHandoffStatePayload> {
  const payload = await verifyPayload<OAuthLocalHandoffStatePayload>(token, secret, "state");
  if (!isOAuthLocalHandoffStatePayload(payload)) {
    throw badRequest("invalid_state", "OAuth state payload is invalid");
  }
  if (payload.exp <= now) {
    throw unauthorized("expired_state", "OAuth state has expired");
  }
  return payload;
}

export function nowSeconds(): number {
  return Math.floor(Date.now() / 1000);
}

async function signPayload(payload: unknown, secret: string): Promise<string> {
  const body = utf8Base64Url(JSON.stringify(payload));
  const signature = await hmacSha256Base64Url(secret, body);
  return `${body}.${signature}`;
}

async function verifyPayload<T>(token: string, secret: string, label: "session" | "state"): Promise<T> {
  const [body, signature] = token.split(".");
  if (!body || !signature) {
    throw badRequest(`invalid_${label}`, `OAuth ${label} token is malformed`);
  }
  const expected = await hmacSha256Base64Url(secret, body);
  if (!constantTimeEqual(signature, expected)) {
    throw unauthorized(`invalid_${label}`, `OAuth ${label} token signature is invalid`);
  }
  try {
    return JSON.parse(parseUtf8Base64Url(body)) as T;
  } catch {
    throw badRequest(`invalid_${label}`, `OAuth ${label} token payload is invalid`);
  }
}

function isOAuthSessionPayload(value: unknown): value is OAuthSessionPayload {
  if (!value || typeof value !== "object") {
    return false;
  }
  const payload = value as Partial<OAuthSessionPayload>;
  return (
    payload.v === 1 &&
    isConnectorId(payload.connector) &&
    typeof payload.state === "string" &&
    typeof payload.redirect_uri === "string" &&
    typeof payload.iat === "number" &&
    typeof payload.exp === "number" &&
    typeof payload.nonce === "string"
  );
}

function isOAuthLocalHandoffStatePayload(value: unknown): value is OAuthLocalHandoffStatePayload {
  if (!value || typeof value !== "object") {
    return false;
  }
  const payload = value as Partial<OAuthLocalHandoffStatePayload>;
  return (
    payload.v === 1 &&
    payload.kind === "local_handoff" &&
    isConnectorId(payload.connector) &&
    typeof payload.local_redirect_uri === "string" &&
    typeof payload.provider_redirect_uri === "string" &&
    typeof payload.iat === "number" &&
    typeof payload.exp === "number" &&
    typeof payload.nonce === "string"
  );
}

function isConnectorId(value: unknown): value is ConnectorId {
  return (
    value === "notion" ||
    value === "google-docs" ||
    value === "google-calendar" ||
    value === "gmail" ||
    value === "slack"
  );
}
```

- [ ] **Step 4: Run the signed-state tests**

Run:

```bash
npm --prefix apps/oauth-service test -- local-handoff-state.test.ts
```

Expected: PASS for all three tests.

- [ ] **Step 5: Run broker typecheck**

Run:

```bash
npm --prefix apps/oauth-service run typecheck
```

Expected: PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add apps/oauth-service/src/security/session.ts apps/oauth-service/test/local-handoff-state.test.ts
git commit -m "feat(oauth): sign local handoff state"
```

Expected: commit succeeds.

---

### Task 2: Add Hosted Notion Callback Handoff In The OAuth Service

**Files:**
- Modify: `apps/oauth-service/src/types.ts`
- Modify: `apps/oauth-service/src/security/redirects.ts`
- Modify: `apps/oauth-service/src/app.ts`
- Modify: `apps/oauth-service/test/app.test.ts`

- [ ] **Step 1: Write failing broker tests for hosted Notion start, callback, and exchange**

Append these tests inside `describe("auth broker", () => { ... })` in `apps/oauth-service/test/app.test.ts`.

```ts
  it("starts Notion OAuth with hosted provider callback and local loopback handoff", async () => {
    const hostedEnv: BrokerEnv = {
      ...env,
      LOCALITY_NOTION_REDIRECT_URIS: "http://localhost:8757/oauth/notion/callback",
      LOCALITY_NOTION_HOSTED_CALLBACK_URI: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
    };

    const response = await app.request("/v1/oauth/notion/start", { method: "POST" }, hostedEnv);

    expect(response.status).toBe(200);
    const body = (await response.json()) as StartResponse & {
      authorization_redirect_uri: string;
      exchange_redirect_uri: string;
    };
    expect(body.redirect_uri).toBe("http://localhost:8757/oauth/notion/callback");
    expect(body.authorization_redirect_uri).toBe("https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback");
    expect(body.exchange_redirect_uri).toBe("https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback");
    const authorizationUrl = new URL(body.authorization_url);
    expect(authorizationUrl.searchParams.get("redirect_uri")).toBe(
      "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
    );
    expect(authorizationUrl.searchParams.get("state")).toBe(body.state);
    expect(body.session).toBeTruthy();
    expect(body.state).toBeTruthy();
    expect(body.session).not.toBe(body.state);
  });

  it("redirects a valid hosted Notion callback to the local loopback listener", async () => {
    const hostedEnv: BrokerEnv = {
      ...env,
      LOCALITY_NOTION_REDIRECT_URIS: "http://localhost:8757/oauth/notion/callback",
      LOCALITY_NOTION_HOSTED_CALLBACK_URI: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
    };
    const startResponse = await app.request("/v1/oauth/notion/start", { method: "POST" }, hostedEnv);
    const start = (await startResponse.json()) as StartResponse;

    const callback = await app.request(
      `/v1/oauth/notion/callback?code=authorization-code&state=${encodeURIComponent(start.state)}`,
      { method: "GET" },
      hostedEnv
    );

    expect(callback.status).toBe(303);
    expect(callback.headers.get("cache-control")).toBe("no-store");
    expect(callback.headers.get("referrer-policy")).toBe("no-referrer");
    const location = new URL(callback.headers.get("location") ?? "");
    expect(location.origin).toBe("http://localhost:8757");
    expect(location.pathname).toBe("/oauth/notion/callback");
    expect(location.searchParams.get("code")).toBe("authorization-code");
    expect(location.searchParams.get("state")).toBe(start.state);
  });

  it("redirects hosted Notion provider denial to the local listener with state", async () => {
    const hostedEnv: BrokerEnv = {
      ...env,
      LOCALITY_NOTION_REDIRECT_URIS: "http://localhost:8757/oauth/notion/callback",
      LOCALITY_NOTION_HOSTED_CALLBACK_URI: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
    };
    const startResponse = await app.request("/v1/oauth/notion/start", { method: "POST" }, hostedEnv);
    const start = (await startResponse.json()) as StartResponse;

    const callback = await app.request(
      `/v1/oauth/notion/callback?error=access_denied&error_description=User%20cancelled&state=${encodeURIComponent(start.state)}`,
      { method: "GET" },
      hostedEnv
    );

    expect(callback.status).toBe(303);
    const location = new URL(callback.headers.get("location") ?? "");
    expect(location.origin).toBe("http://localhost:8757");
    expect(location.pathname).toBe("/oauth/notion/callback");
    expect(location.searchParams.get("error")).toBe("access_denied");
    expect(location.searchParams.get("error_description")).toBe("User cancelled");
    expect(location.searchParams.get("state")).toBe(start.state);
    expect(location.searchParams.get("code")).toBeNull();
  });

  it("rejects hosted Notion callback state that was not signed by the broker", async () => {
    const hostedEnv: BrokerEnv = {
      ...env,
      LOCALITY_NOTION_REDIRECT_URIS: "http://localhost:8757/oauth/notion/callback",
      LOCALITY_NOTION_HOSTED_CALLBACK_URI: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
    };

    const callback = await app.request(
      "/v1/oauth/notion/callback?code=authorization-code&state=not-signed",
      { method: "GET" },
      hostedEnv
    );

    expect(callback.status).toBe(400);
    await expect(callback.json()).resolves.toMatchObject({
      error: { code: "invalid_state" }
    });
  });

  it("exchanges hosted Notion authorization codes with the hosted redirect URI", async () => {
    const hostedEnv: BrokerEnv = {
      ...env,
      LOCALITY_NOTION_REDIRECT_URIS: "http://localhost:8757/oauth/notion/callback",
      LOCALITY_NOTION_HOSTED_CALLBACK_URI: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
    };
    const startResponse = await app.request("/v1/oauth/notion/start", { method: "POST" }, hostedEnv);
    const start = (await startResponse.json()) as StartResponse & { exchange_redirect_uri: string };
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
          redirect_uri: start.exchange_redirect_uri
        })
      },
      hostedEnv
    );

    expect(response.status).toBe(200);
    const notionRequest = JSON.parse((fetchMock.mock.calls[0]?.[1] as RequestInit).body as string);
    expect(notionRequest).toMatchObject({
      grant_type: "authorization_code",
      code: "authorization-code",
      redirect_uri: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
    });
  });
```

- [ ] **Step 2: Run the hosted broker tests and verify they fail**

Run:

```bash
npm --prefix apps/oauth-service test -- app.test.ts
```

Expected: FAIL because `LOCALITY_NOTION_HOSTED_CALLBACK_URI`, `authorization_redirect_uri`, `exchange_redirect_uri`, and `GET /v1/oauth/notion/callback` are not implemented.

- [ ] **Step 3: Add the hosted callback env type**

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
  LOCALITY_GOOGLE_DOCS_AUTH_BASE_URL?: string;
  LOCALITY_GOOGLE_DOCS_API_BASE_URL?: string;
  LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS?: string;
  LOCALITY_GOOGLE_CALENDAR_AUTH_BASE_URL?: string;
  LOCALITY_GOOGLE_CALENDAR_API_BASE_URL?: string;
  LOCALITY_GMAIL_REDIRECT_URIS?: string;
  LOCALITY_GMAIL_AUTH_BASE_URL?: string;
  LOCALITY_GMAIL_API_BASE_URL?: string;
  LOCALITY_SLACK_CLIENT_ID?: string;
  LOCALITY_SLACK_CLIENT_SECRET?: string;
  LOCALITY_SLACK_REDIRECT_URIS?: string;
  LOCALITY_SLACK_AUTH_BASE_URL?: string;
  LOCALITY_SLACK_API_BASE_URL?: string;
}
```

- [ ] **Step 4: Add hosted callback validation without weakening loopback validation**

Modify `apps/oauth-service/src/security/redirects.ts` by adding these exports and helpers. Keep existing `validateNotionRedirectUri` loopback-only.

```ts
const NOTION_HOSTED_CALLBACK_PATH = "/v1/oauth/notion/callback";

export function hostedNotionCallbackUri(env: BrokerEnv): string | undefined {
  const value = env.LOCALITY_NOTION_HOSTED_CALLBACK_URI?.trim();
  if (!value) {
    return undefined;
  }
  return validateHostedNotionCallbackUri(value);
}

export function validateHostedNotionCallbackUri(callbackUri: string): string {
  let parsed: URL;
  try {
    parsed = new URL(callbackUri);
  } catch {
    throw badRequest("invalid_hosted_callback_uri", "hosted Notion callback URI must be a valid URL");
  }
  if (
    parsed.protocol !== "https:" ||
    parsed.username !== "" ||
    parsed.password !== "" ||
    parsed.hostname === "" ||
    parsed.port !== "" ||
    parsed.pathname !== NOTION_HOSTED_CALLBACK_PATH ||
    parsed.search !== "" ||
    parsed.hash !== ""
  ) {
    throw badRequest(
      "invalid_hosted_callback_uri",
      "hosted Notion callback URI must be an HTTPS URL at /v1/oauth/notion/callback without userinfo, port, query, or fragment"
    );
  }
  return parsed.toString();
}

export function validateNotionExchangeRedirectUri(env: BrokerEnv, redirectUri: string): string {
  const hosted = hostedNotionCallbackUri(env);
  if (hosted && redirectUri === hosted) {
    return redirectUri;
  }
  return validateNotionRedirectUri(env, redirectUri);
}
```

- [ ] **Step 5: Add Notion start redirect selection and callback helpers**

Modify imports at the top of `apps/oauth-service/src/app.ts`:

```ts
import { randomBase64Url, decryptJsonHandle, encryptJsonHandle } from "./security/crypto";
import {
  nowSeconds,
  signLocalHandoffState,
  signSession,
  verifyLocalHandoffState,
  verifySession
} from "./security/session";
import {
  hostedNotionCallbackUri,
  validateGmailRedirectUri,
  validateGoogleCalendarRedirectUri,
  validateGoogleDocsRedirectUri,
  validateNotionExchangeRedirectUri,
  validateNotionRedirectUri,
  validateSlackRedirectUri
} from "./security/redirects";
```

Add these interfaces near the existing request interfaces:

```ts
interface NotionStartRedirects {
  localRedirectUri: string;
  authorizationRedirectUri: string;
  exchangeRedirectUri: string;
  hostedHandoff: boolean;
}

interface HostedCallbackQuery {
  state?: string;
  code?: string;
  error?: string;
  error_description?: string;
}
```

Add these helper functions near `requireString`:

```ts
function notionStartRedirects(env: BrokerEnv, requestedRedirectUri: string): NotionStartRedirects {
  const localRedirectUri = validateNotionRedirectUri(env, requestedRedirectUri);
  const hostedCallbackUri = hostedNotionCallbackUri(env);
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
```

- [ ] **Step 6: Implement hosted Notion start behavior**

Replace the current `/v1/oauth/notion/start` route in `apps/oauth-service/src/app.ts` with:

```ts
app.post("/v1/oauth/notion/start", async (c) => {
  const body = await optionalJson<StartRequest>(c.req.raw);
  const redirects = notionStartRedirects(
    c.env,
    body.redirect_uri ?? "http://localhost:8757/oauth/notion/callback"
  );
  const now = nowSeconds();
  const state = redirects.hostedHandoff
    ? await signLocalHandoffState(
        {
          v: 1,
          kind: "local_handoff",
          connector: "notion",
          local_redirect_uri: redirects.localRedirectUri,
          provider_redirect_uri: redirects.authorizationRedirectUri,
          iat: now,
          exp: now + SESSION_TTL_SECONDS,
          nonce: randomBase64Url()
        },
        requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
      )
    : randomBase64Url();
  const session = await signSession(
    {
      v: 1,
      connector: "notion",
      state,
      redirect_uri: redirects.exchangeRedirectUri,
      iat: now,
      exp: now + SESSION_TTL_SECONDS,
      nonce: randomBase64Url()
    },
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  return c.json({
    connector: "notion",
    client_id: c.env.LOCALITY_NOTION_CLIENT_ID,
    authorization_url: notionAuthorizeUrl(c.env, redirects.authorizationRedirectUri, state),
    redirect_uri: redirects.localRedirectUri,
    authorization_redirect_uri: redirects.authorizationRedirectUri,
    exchange_redirect_uri: redirects.exchangeRedirectUri,
    session,
    state,
    expires_in: SESSION_TTL_SECONDS
  });
});
```

- [ ] **Step 7: Implement hosted Notion callback route**

Add this route after `/v1/oauth/notion/start` and before `/v1/oauth/notion/exchange`:

```ts
app.get("/v1/oauth/notion/callback", async (c) => {
  const query = c.req.query() as HostedCallbackQuery;
  const state = callbackString(query.state, "state", 8192);
  const payload = await verifyLocalHandoffState(
    state,
    requireOperationalSecret(c.env.LOCALITY_BROKER_SESSION_SECRET, "LOCALITY_BROKER_SESSION_SECRET")
  );
  if (payload.connector !== "notion") {
    throw badRequest("invalid_state", "OAuth state connector is invalid");
  }
  const expectedProviderRedirectUri = hostedNotionCallbackUri(c.env);
  if (!expectedProviderRedirectUri || payload.provider_redirect_uri !== expectedProviderRedirectUri) {
    throw badRequest("invalid_state", "OAuth state provider redirect is invalid");
  }
  const localRedirectUri = validateNotionRedirectUri(c.env, payload.local_redirect_uri);
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
});
```

- [ ] **Step 8: Allow Notion exchange to use hosted exchange redirect URI**

Replace the redirect validation line inside `/v1/oauth/notion/exchange`:

```ts
const redirectUri = validateNotionRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
```

with:

```ts
const redirectUri = validateNotionExchangeRedirectUri(c.env, requireString(body.redirect_uri, "redirect_uri"));
```

Keep the existing session check:

```ts
if (payload.connector !== "notion" || payload.state !== state || payload.redirect_uri !== redirectUri) {
  throw badRequest("oauth_session_mismatch", "OAuth callback did not match the broker session");
}
```

- [ ] **Step 9: Run the hosted broker tests**

Run:

```bash
npm --prefix apps/oauth-service test -- app.test.ts
```

Expected: PASS.

- [ ] **Step 10: Run the OAuth service checks**

Run:

```bash
npm --prefix apps/oauth-service run check
```

Expected: PASS for `tsc --noEmit` and `vitest run`.

- [ ] **Step 11: Commit**

Run:

```bash
git add apps/oauth-service/src/types.ts apps/oauth-service/src/security/redirects.ts apps/oauth-service/src/app.ts apps/oauth-service/test/app.test.ts
git commit -m "feat(oauth): hand hosted notion callback to localhost"
```

Expected: commit succeeds.

---

### Task 3: Teach The Rust Notion Client About Separate Redirect URIs

**Files:**
- Modify: `crates/locality-notion/src/oauth.rs`
- Modify: `crates/loc-cli/src/commands.rs`
- Modify: `crates/loc-cli/tests/connect.rs`

- [ ] **Step 1: Write failing Rust tests for hosted redirect fields**

In `crates/locality-notion/src/oauth.rs`, add this test near the existing broker start response tests:

```rust
    #[test]
    fn broker_start_response_uses_hosted_authorization_redirect_when_present() {
        let start = NotionOAuthBrokerStartResponse {
            connector: "notion".to_string(),
            client_id: "client-id".to_string(),
            authorization_url: "https://api.notion.com/v1/oauth/authorize?client_id=wrong"
                .to_string(),
            redirect_uri: "http://localhost:8757/oauth/notion/callback".to_string(),
            authorization_redirect_uri: Some(
                "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
                    .to_string(),
            ),
            exchange_redirect_uri: Some(
                "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
                    .to_string(),
            ),
            session: "session-1".to_string(),
            state: "state-1".to_string(),
            expires_in: 300,
        };

        let url = Url::parse(&start.normalized_authorization_url()).expect("normalized URL");

        assert_eq!(
            query_value(&url, "redirect_uri").as_deref(),
            Some("https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback")
        );
        assert_eq!(
            start.local_redirect_uri(),
            "http://localhost:8757/oauth/notion/callback"
        );
        assert_eq!(
            start.exchange_redirect_uri(),
            "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
        );
    }
```

In `crates/loc-cli/tests/connect.rs`, change `FakeBrokerOAuthExchange` into a configurable fake:

```rust
#[derive(Clone, Debug)]
struct FakeBrokerOAuthExchange {
    expected_redirect_uri: &'static str,
}

impl Default for FakeBrokerOAuthExchange {
    fn default() -> Self {
        Self {
            expected_redirect_uri: "http://localhost:8757/oauth/notion/callback",
        }
    }
}
```

Then update its assertion:

```rust
        assert_eq!(request.redirect_uri, self.expected_redirect_uri);
```

Update the existing broker OAuth test setup:

```rust
    let exchange = FakeBrokerOAuthExchange::default();
```

Add this test next to the existing Notion broker OAuth connect tests:

```rust
#[test]
fn connect_notion_broker_oauth_can_store_local_credentials_after_hosted_exchange_redirect() {
    let mut store = InMemoryStateStore::new();
    let credentials = InMemoryCredentialStore::new();

    let report = run_connect_notion_broker_oauth(
        &mut store,
        &credentials,
        BrokerOAuthConnectOptions {
            connection_id: Some(ConnectionId::new("notion-hosted")),
            broker_url: "https://afs-oauth-broker.saurabh-b07.workers.dev".to_string(),
            client_id: "client-id".to_string(),
            session: "broker-session".to_string(),
            state: "state-1".to_string(),
            code: "oauth-code".to_string(),
            redirect_uri: "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
                .to_string(),
        },
        &FakeBrokerOAuthExchange {
            expected_redirect_uri:
                "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback",
        },
    )
    .expect("broker OAuth connect");

    assert_eq!(report.connection_id, "notion-hosted");
    assert_eq!(report.auth_kind, "oauth");
    let saved = store
        .get_connection(&ConnectionId::new("notion-hosted"))
        .expect("get connection")
        .expect("saved connection");
    assert_eq!(saved.auth_kind, "oauth");
    assert_eq!(saved.secret_ref, "connection:notion-hosted");
    let secret = credentials
        .get("connection:notion-hosted")
        .expect("credential saved");
    assert!(secret.contains("\"oauth_broker_url\":\"https://afs-oauth-broker.saurabh-b07.workers.dev\""));
    assert!(secret.contains("\"refresh_token_handle\":\"opaque-refresh-handle\""));
}
```

- [ ] **Step 2: Run the failing Rust tests**

Run:

```bash
cargo test -p locality-notion broker_start_response_uses_hosted_authorization_redirect_when_present
cargo test -p loc-cli connect_notion_broker_oauth_can_store_local_credentials_after_hosted_exchange_redirect
```

Expected: the `locality-notion` test fails because the response struct has no hosted redirect fields or accessors. The `loc-cli` test may fail to compile until the fake exchange change is made.

- [ ] **Step 3: Extend the Notion broker start response**

Modify `NotionOAuthBrokerStartResponse` in `crates/locality-notion/src/oauth.rs`:

```rust
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct NotionOAuthBrokerStartResponse {
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
```

Update its `fmt::Debug` implementation:

```rust
impl fmt::Debug for NotionOAuthBrokerStartResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NotionOAuthBrokerStartResponse")
            .field("connector", &self.connector)
            .field("client_id", &self.client_id)
            .field("authorization_url", &REDACTED)
            .field("redirect_uri", &self.redirect_uri)
            .field("authorization_redirect_uri", &self.authorization_redirect_uri)
            .field("exchange_redirect_uri", &self.exchange_redirect_uri)
            .field("session", &REDACTED)
            .field("state", &REDACTED)
            .field("expires_in", &self.expires_in)
            .finish()
    }
}
```

Update its methods:

```rust
impl NotionOAuthBrokerStartResponse {
    pub fn normalized_authorization_url(&self) -> String {
        normalize_notion_authorization_url(
            &self.authorization_url,
            &self.client_id,
            self.authorization_redirect_uri(),
            &self.state,
        )
    }

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

Update the two existing struct literals found by:

```bash
rg -n "NotionOAuthBrokerStartResponse \\{" crates/locality-notion/src/oauth.rs
```

Add these two fields to each existing test literal:

```rust
            authorization_redirect_uri: None,
            exchange_redirect_uri: None,
```

Update the exact debug-output assertion in `crates/locality-notion/src/oauth.rs` to include the two new fields:

```rust
        assert_eq!(
            format!("{start:?}"),
            "NotionOAuthBrokerStartResponse { connector: \"notion\", client_id: \"client-id\", authorization_url: \"<redacted>\", redirect_uri: \"http://localhost/callback\", authorization_redirect_uri: None, exchange_redirect_uri: None, session: \"<redacted>\", state: \"<redacted>\", expires_in: 300 }"
        );
```

- [ ] **Step 4: Use the hosted exchange redirect while listening locally**

Modify `crates/loc-cli/src/commands.rs` in `run_connect_notion_command`. Keep the local listener on `start.redirect_uri`; only change the exchange option.

Replace:

```rust
        let options = BrokerOAuthConnectOptions {
            connection_id: flag_value(args, "--name").map(ConnectionId::new),
            broker_url: broker_config.broker_url,
            client_id: start.client_id,
            session: start.session,
            state: start.state,
            code: authorization.code,
            redirect_uri: start.redirect_uri,
        };
```

with:

```rust
        let exchange_redirect_uri = start.exchange_redirect_uri().to_string();
        let options = BrokerOAuthConnectOptions {
            connection_id: flag_value(args, "--name").map(ConnectionId::new),
            broker_url: broker_config.broker_url,
            client_id: start.client_id,
            session: start.session,
            state: start.state,
            code: authorization.code,
            redirect_uri: exchange_redirect_uri,
        };
```

The local listener call remains:

```rust
        let authorization = match run_local_oauth_authorization(
            "Notion",
            &authorization_url,
            &start.redirect_uri,
            &start.state,
            has_flag(args, "--no-browser"),
            json,
        ) {
```

- [ ] **Step 5: Run the focused Rust tests**

Run:

```bash
cargo test -p locality-notion broker_start_response_uses_hosted_authorization_redirect_when_present
cargo test -p loc-cli connect_notion_broker_oauth_can_store_local_credentials_after_hosted_exchange_redirect
```

Expected: PASS.

- [ ] **Step 6: Run broader affected Rust tests**

Run:

```bash
cargo test -p locality-notion oauth::tests
cargo test -p loc-cli connect
```

Expected: PASS.

- [ ] **Step 7: Commit**

Run:

```bash
git add crates/locality-notion/src/oauth.rs crates/loc-cli/src/commands.rs crates/loc-cli/tests/connect.rs
git commit -m "feat(cli): support hosted notion broker callback"
```

Expected: commit succeeds.

---

### Task 4: Update OAuth Service Deployment Config And Documentation

**Files:**
- Modify: `apps/oauth-service/wrangler.toml`
- Modify: `apps/oauth-service/README.md`
- Modify: `apps/oauth-service/docs/security.md`
- Modify: `docs/cli.md`
- Modify: `docs-site/cli-reference.mdx`

- [ ] **Step 1: Update Worker environment config**

Modify `[vars]` in `apps/oauth-service/wrangler.toml`.

Replace the Notion redirect config:

```toml
LOCALITY_NOTION_REDIRECT_URIS = "http://localhost:8757/oauth/notion/callback,http://127.0.0.1:8757/oauth/notion/callback,https://api.dev.locality.dev/v1/oauth/notion/callback"
```

with:

```toml
LOCALITY_NOTION_REDIRECT_URIS = "http://localhost:8757/oauth/notion/callback,http://127.0.0.1:8757/oauth/notion/callback"
LOCALITY_NOTION_HOSTED_CALLBACK_URI = "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback"
```

Register that exact `LOCALITY_NOTION_HOSTED_CALLBACK_URI` value in the Notion public integration's OAuth redirect URI settings before deploying the Worker change.

- [ ] **Step 2: Document the hosted handoff API response**

Update `apps/oauth-service/README.md` under `POST /v1/oauth/notion/start`.

Replace the response example with:

```json
{
  "connector": "notion",
  "client_id": "public-client-id",
  "authorization_url": "https://api.notion.com/v1/oauth/authorize?...",
  "redirect_uri": "http://localhost:8757/oauth/notion/callback",
  "authorization_redirect_uri": "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback",
  "exchange_redirect_uri": "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback",
  "session": "signed-session",
  "state": "signed-local-handoff-state",
  "expires_in": 600
}
```

Add this paragraph immediately below the example:

```md
When `LOCALITY_NOTION_HOSTED_CALLBACK_URI` is set, `redirect_uri` remains the local loopback URI where the CLI listens. `authorization_redirect_uri` and `exchange_redirect_uri` are the HTTPS provider callback URI registered with Notion. The browser first returns to the broker callback, and the broker redirects the browser to `redirect_uri` with the provider code or error. The CLI then exchanges the code using `exchange_redirect_uri`, so Notion sees the same redirect URI during authorization and token exchange.
```

Add this API section after the Notion start section:

```md
### `GET /v1/oauth/notion/callback`

This browser-facing route is used only when `LOCALITY_NOTION_HOSTED_CALLBACK_URI`
is configured. It accepts Notion's `code` and `state`, verifies the signed
local-handoff state, and returns `303 See Other` to the loopback callback held
inside that state.

Success redirects to:

```text
http://localhost:8757/oauth/notion/callback?state=...&code=...
```

Provider denial redirects to:

```text
http://localhost:8757/oauth/notion/callback?state=...&error=access_denied&error_description=...
```

The route sets `Cache-Control: no-store` and `Referrer-Policy: no-referrer`.
It does not persist provider codes, tokens, refresh handles, or local callback
URIs.
```

- [ ] **Step 3: Document redirect security boundaries**

Update `apps/oauth-service/docs/security.md` under `## Redirects`.

Replace the current section body with:

```md
The broker keeps two Notion redirect boundaries separate:

- `LOCALITY_NOTION_REDIRECT_URIS` is a loopback-only allowlist for local CLI callbacks such as `http://localhost:8757/oauth/notion/callback`.
- `LOCALITY_NOTION_HOSTED_CALLBACK_URI` is one exact HTTPS callback served by this broker at `/v1/oauth/notion/callback`.

When hosted handoff is enabled, the Notion authorization request uses the hosted
callback URI. The callback route verifies a signed state payload before
redirecting to a loopback URI from the allowlist. The token exchange also uses
the hosted callback URI so the provider sees the same redirect URI in both OAuth
steps.

Google Docs, Google Calendar, Gmail, and Slack continue to accept only their
configured loopback callback URLs in this implementation.
```

- [ ] **Step 4: Document the CLI-visible behavior**

Update the Notion OAuth paragraph in `docs/cli.md`.

Replace:

```md
The default broker is `https://afs-oauth-broker.saurabh-b07.workers.dev`; override it with `--broker-url <url>`, `LOCALITY_NOTION_OAUTH_BROKER_URL`, or `LOCALITY_AUTH_BROKER_URL`. The default callback is `http://localhost:8757/oauth/notion/callback`; override it with `--redirect-uri <uri>` or `LOCALITY_NOTION_OAUTH_REDIRECT_URI`. The redirect URI must be registered on the Notion public integration.
```

with:

```md
The default broker is `https://afs-oauth-broker.saurabh-b07.workers.dev`; override it with `--broker-url <url>`, `LOCALITY_NOTION_OAUTH_BROKER_URL`, or `LOCALITY_AUTH_BROKER_URL`. The default local callback is `http://localhost:8757/oauth/notion/callback`; override it with `--redirect-uri <uri>` or `LOCALITY_NOTION_OAUTH_REDIRECT_URI`. In production the broker may use its own HTTPS provider callback registered on the Notion public integration, then hand the browser back to the local callback. The command still stores the resulting OAuth credential locally.
```

Make the same Notion paragraph replacement in `docs-site/cli-reference.mdx`.

- [ ] **Step 5: Run docs and service checks**

Run:

```bash
npm --prefix apps/oauth-service run check
cargo test -p locality-notion oauth::tests
cargo test -p loc-cli connect
```

Expected: all commands PASS.

- [ ] **Step 6: Commit**

Run:

```bash
git add apps/oauth-service/wrangler.toml apps/oauth-service/README.md apps/oauth-service/docs/security.md docs/cli.md docs-site/cli-reference.mdx
git commit -m "docs(oauth): describe hosted notion handoff"
```

Expected: commit succeeds.

---

### Task 5: End-To-End Verification And Deployment Gate

**Files:**
- No source edits expected in this task.

- [ ] **Step 1: Run the complete OAuth service suite**

Run:

```bash
npm --prefix apps/oauth-service run check
```

Expected: PASS.

- [ ] **Step 2: Run focused Rust suites**

Run:

```bash
cargo test -p locality-notion oauth::tests
cargo test -p loc-cli connect
```

Expected: PASS.

- [ ] **Step 3: Run repo-level checks that include OAuth service**

Run:

```bash
make check-oauth-service
```

Expected: PASS.

- [ ] **Step 4: Run a local Worker smoke test**

Prepare local Worker variables. `apps/oauth-service/.dev.vars` is ignored by git. If the file already exists, preserve it first:

```bash
if [ -f apps/oauth-service/.dev.vars ]; then
  cp apps/oauth-service/.dev.vars apps/oauth-service/.dev.vars.before-hosted-handoff-smoke
fi
cat > apps/oauth-service/.dev.vars <<'EOF'
LOCALITY_BROKER_SESSION_SECRET=test-session-secret-with-enough-entropy
LOCALITY_REFRESH_HANDLE_KEY=test-refresh-handle-key-with-enough-entropy
LOCALITY_TOKEN_MODE=handle
LOCALITY_NOTION_CLIENT_ID=notion-client-id
LOCALITY_NOTION_CLIENT_SECRET=notion-client-secret
LOCALITY_NOTION_REDIRECT_URIS=http://localhost:8757/oauth/notion/callback,http://127.0.0.1:8757/oauth/notion/callback
LOCALITY_NOTION_HOSTED_CALLBACK_URI=https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback
EOF
```

Start the Worker in one terminal:

```bash
npm --prefix apps/oauth-service run dev
```

In another terminal, request a start session:

```bash
curl --fail --silent --show-error \
  --request POST \
  --header 'content-type: application/json' \
  --data '{"redirect_uri":"http://localhost:8757/oauth/notion/callback"}' \
  http://127.0.0.1:8787/v1/oauth/notion/start | jq .
```

Expected JSON shape:

```json
{
  "connector": "notion",
  "client_id": "notion-client-id",
  "authorization_url": "https://api.notion.com/v1/oauth/authorize?...",
  "redirect_uri": "http://localhost:8757/oauth/notion/callback",
  "authorization_redirect_uri": "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback",
  "exchange_redirect_uri": "https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback",
  "session": "...",
  "state": "...",
  "expires_in": 600
}
```

The `authorization_url` must contain:

```text
redirect_uri=https%3A%2F%2Fafs-oauth-broker.saurabh-b07.workers.dev%2Fv1%2Foauth%2Fnotion%2Fcallback
```

- [ ] **Step 5: Run a local CLI smoke test against the dev Worker**

With the Worker still running, run:

```bash
LOCALITY_NOTION_OAUTH_BROKER_URL=http://127.0.0.1:8787 \
  cargo run -p loc-cli -- connect notion --name hosted-handoff-smoke --no-browser
```

Expected:

- The command prints an authorization URL instead of opening the browser.
- The printed authorization URL uses the hosted HTTPS callback in its `redirect_uri` query parameter.
- The command waits for `http://localhost:8757/oauth/notion/callback`.
- Stop the command with Ctrl-C after verifying the URL and listener output.

Stop the Worker and restore any previous local variables:

```bash
rm -f apps/oauth-service/.dev.vars
if [ -f apps/oauth-service/.dev.vars.before-hosted-handoff-smoke ]; then
  mv apps/oauth-service/.dev.vars.before-hosted-handoff-smoke apps/oauth-service/.dev.vars
fi
```

- [ ] **Step 6: Verify provider registration before deploy**

Open the Notion public integration settings and verify the OAuth redirect URI list contains exactly this URI for the Worker deployment in `apps/oauth-service/wrangler.toml`:

```text
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/notion/callback
```

The Notion redirect URI list no longer needs either loopback HTTP URI for the production public integration when hosted handoff is enabled.

- [ ] **Step 7: Commit any verification-only documentation correction**

If the smoke test finds that the deployed OAuth service origin differs from `https://afs-oauth-broker.saurabh-b07.workers.dev`, update these files with the exact deployed origin before deployment:

```text
apps/oauth-service/wrangler.toml
apps/oauth-service/README.md
apps/oauth-service/docs/security.md
docs/cli.md
docs-site/cli-reference.mdx
```

Then run:

```bash
npm --prefix apps/oauth-service run check
cargo test -p locality-notion oauth::tests
cargo test -p loc-cli connect
git add apps/oauth-service/wrangler.toml apps/oauth-service/README.md apps/oauth-service/docs/security.md docs/cli.md docs-site/cli-reference.mdx
git commit -m "chore(oauth): align hosted callback origin"
```

Expected: checks pass and commit succeeds when a correction was needed. If no correction was needed, leave the worktree unchanged.

---

## Self-Review

Spec coverage:

- HTTPS provider callback requirement: Task 2 and Task 4.
- Keep local credentials and the current CLI flow: Task 3 and Task 5.
- OAuth service is already deployed with TLS: Task 4 uses the deployed Worker callback URI and keeps internal API out of the local-credential path.
- Provider token exchange uses the same redirect URI as authorization: Task 2 and Task 3.
- No backend-hosted credential migration: excluded in Scope and no private backend files are modified.

Placeholder scan:

- No placeholder markers remain in the plan.
- Every code-changing step includes concrete code or exact replacements.
- Every verification step has an exact command and expected result.

Type consistency:

- `authorization_redirect_uri` and `exchange_redirect_uri` are snake_case JSON fields in the Worker and Rust response struct.
- Rust accessors are `authorization_redirect_uri()`, `exchange_redirect_uri()`, and `local_redirect_uri()`.
- Worker env field is consistently `LOCALITY_NOTION_HOSTED_CALLBACK_URI`.
