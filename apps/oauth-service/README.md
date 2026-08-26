# Locality Auth Broker

Minimal OAuth broker for Locality connector auth.

The broker exists for providers whose OAuth REST API requires a confidential
client secret. The local `loc` client keeps the normal desktop UX: start a
localhost callback, open the provider consent page, receive the authorization
code, and store returned credentials in the OS credential store. This service
only performs the confidential token exchange and refresh calls.

## Flow

```text
loc CLI -> broker /start with localhost client redirect_uri
broker -> loc CLI with authorization_url, session, state, provider_redirect_uri
browser -> provider consent using provider_redirect_uri=https://<broker>/v1/oauth/<connector>/callback
provider -> broker HTTPS callback with code/state
broker -> localhost client redirect_uri with code/state
loc CLI -> broker /exchange with code, state, session, and localhost client redirect_uri
broker -> provider token endpoint with provider_redirect_uri and client_secret
broker -> loc CLI with access token and refresh handle

later:
loc CLI -> broker /refresh with refresh_token_handle
broker -> provider token endpoint with client_secret
broker -> loc CLI with new access token and new refresh handle
```

Slack refresh tokens rotate on every exchange and are single-use. Slack handle
refreshes are coordinated by a Durable Object keyed by the opaque handle. The
broker persists an encrypted successful response for ten minutes before
returning it, so a retry after a lost response receives the same rotated token
pair without consuming the old Slack refresh token twice. Concurrent requests
for the same handle are coalesced.

Provider OAuth apps should register only the broker HTTPS callback URLs, such as
`https://oauth.locality.example/v1/oauth/notion/callback`. The localhost URL is
only the desktop completion URL used after the broker receives and verifies the
provider callback.

The broker uses the signed session token as the OAuth `state`, so `session` and
`state` match in `/start` and `/exchange` payloads.

The broker does not persist page content. In `handle` mode, it returns an
encrypted opaque refresh handle instead of the raw provider refresh token.
The Slack refresh coordinator temporarily persists only an encrypted successful
refresh response for retry recovery; its alarm removes that response after ten
minutes.

## API

### `POST /v1/oauth/notion/start`

Request:

```json
{
  "redirect_uri": "http://localhost:8757/oauth/notion/callback"
}
```

Response:

```json
{
  "connector": "notion",
  "client_id": "public-client-id",
  "authorization_url": "https://api.notion.com/v1/oauth/authorize?...",
  "redirect_uri": "http://localhost:8757/oauth/notion/callback",
  "provider_redirect_uri": "https://oauth.locality.example/v1/oauth/notion/callback",
  "session": "signed-session",
  "state": "signed-session",
  "expires_in": 600
}
```

### `POST /v1/oauth/notion/exchange`

Request:

```json
{
  "session": "signed-session",
  "state": "signed-session",
  "code": "provider-authorization-code",
  "redirect_uri": "http://localhost:8757/oauth/notion/callback"
}
```

Response includes the provider access token and either `refresh_token_handle` or
`refresh_token`, depending on `LOCALITY_TOKEN_MODE`.

### `POST /v1/oauth/notion/refresh`

Request:

```json
{
  "refresh_token_handle": "locrh_v1..."
}
```

### `POST /v1/oauth/google-docs/start`

Request:

```json
{
  "redirect_uri": "http://localhost:8757/oauth/google-docs/callback"
}
```

The broker requests `openid`, `email`, `profile`,
`https://www.googleapis.com/auth/documents`,
`https://www.googleapis.com/auth/drive.file`, and
`https://www.googleapis.com/auth/drive.metadata.readonly`.

Response:

```json
{
  "connector": "google-docs",
  "client_id": "public-client-id",
  "authorization_url": "https://accounts.google.com/o/oauth2/v2/auth?...",
  "redirect_uri": "http://localhost:8757/oauth/google-docs/callback",
  "provider_redirect_uri": "https://oauth.locality.example/v1/oauth/google-docs/callback",
  "session": "signed-session",
  "state": "signed-session",
  "expires_in": 600
}
```

### `POST /v1/oauth/google-docs/exchange`

Request:

```json
{
  "session": "signed-session",
  "state": "signed-session",
  "code": "provider-authorization-code",
  "redirect_uri": "http://localhost:8757/oauth/google-docs/callback"
}
```

Response includes the Google OAuth access token, granted scopes, optional ID
token, and either `refresh_token_handle` or `refresh_token`, depending on
`LOCALITY_TOKEN_MODE`.

### `POST /v1/oauth/google-docs/refresh`

Request:

```json
{
  "refresh_token_handle": "locrh_v1..."
}
```

### `POST /v1/oauth/google-calendar/start`

Request:

```json
{
  "redirect_uri": "http://localhost:8757/oauth/google-calendar/callback"
}
```

If omitted, the default callback is
`http://localhost:8757/oauth/google-calendar/callback`. The broker requests
`openid`, `email`, `profile`, and
`https://www.googleapis.com/auth/calendar.events.owned`.

Response:

```json
{
  "connector": "google-calendar",
  "client_id": "public-client-id",
  "authorization_url": "https://accounts.google.com/o/oauth2/v2/auth?...",
  "redirect_uri": "http://localhost:8757/oauth/google-calendar/callback",
  "provider_redirect_uri": "https://oauth.locality.example/v1/oauth/google-calendar/callback",
  "session": "signed-session",
  "state": "signed-session",
  "expires_in": 600
}
```

### `POST /v1/oauth/google-calendar/exchange`

Request:

```json
{
  "session": "signed-session",
  "state": "signed-session",
  "code": "provider-authorization-code",
  "redirect_uri": "http://localhost:8757/oauth/google-calendar/callback"
}
```

Response includes the Google OAuth access token for Calendar event scopes,
granted scopes, optional ID token, `workspace_id: "primary"`,
`workspace_name: "Primary calendar"`, and either `refresh_token_handle` or
`refresh_token`, depending on `LOCALITY_TOKEN_MODE`.

### `POST /v1/oauth/google-calendar/refresh`

Request:

```json
{
  "refresh_token_handle": "locrh_v1..."
}
```

### `POST /v1/oauth/gmail/start`

Request:

```json
{
  "redirect_uri": "http://localhost:8757/oauth/gmail/callback"
}
```

Response:

```json
{
  "connector": "gmail",
  "client_id": "public-client-id",
  "authorization_url": "https://accounts.google.com/o/oauth2/v2/auth?...",
  "redirect_uri": "http://localhost:8757/oauth/gmail/callback",
  "provider_redirect_uri": "https://oauth.locality.example/v1/oauth/gmail/callback",
  "session": "signed-session",
  "state": "signed-session",
  "expires_in": 600
}
```

### `POST /v1/oauth/gmail/exchange`

Request:

```json
{
  "session": "signed-session",
  "state": "signed-session",
  "code": "provider-authorization-code",
  "redirect_uri": "http://localhost:8757/oauth/gmail/callback"
}
```

Response includes the Google OAuth access token for Gmail read/compose scopes,
granted scopes, optional ID token, and either `refresh_token_handle` or
`refresh_token`, depending on `LOCALITY_TOKEN_MODE`.

### `POST /v1/oauth/gmail/refresh`

Request:

```json
{
  "refresh_token_handle": "locrh_v1..."
}
```

### `POST /v1/oauth/slack/start`

Request:

```json
{
  "redirect_uri": "http://localhost:8757/oauth/slack/callback"
}
```

Response:

```json
{
  "connector": "slack",
  "client_id": "public-client-id",
  "authorization_url": "https://slack.com/oauth/v2/authorize?...",
  "redirect_uri": "http://localhost:8757/oauth/slack/callback",
  "provider_redirect_uri": "https://oauth.locality.example/v1/oauth/slack/callback",
  "session": "signed-session",
  "state": "signed-session",
  "expires_in": 600
}
```

### `POST /v1/oauth/slack/exchange`

Request:

```json
{
  "session": "signed-session",
  "state": "signed-session",
  "code": "provider-authorization-code",
  "redirect_uri": "http://localhost:8757/oauth/slack/callback"
}
```

Response includes the Slack OAuth access token, granted read-only scopes,
workspace identifiers, bot user ID, and either `refresh_token_handle` or
`refresh_token`, depending on `LOCALITY_TOKEN_MODE`.

### `POST /v1/oauth/slack/refresh`

Request:

```json
{
  "refresh_token_handle": "locrh_v1..."
}
```

## Local Development

```sh
npm install
cp .dev.vars.example .dev.vars
npm run dev
```

Run checks:

```sh
npm run check
```

## Anonymous telemetry ingestion

`POST /v1/telemetry/batch` accepts the versioned, allowlisted Locality desktop
event contract and forwards it to PostHog. The endpoint rejects unknown fields,
free-form values, batches over 50 events, and request bodies over 64 KiB. It does
not accept local log messages, file paths, account data, or content.

Configure `LOCALITY_POSTHOG_PROJECT_KEY` as a Worker secret. Optionally set
`LOCALITY_POSTHOG_HOST`; it defaults to `https://us.i.posthog.com`.

## Required Configuration

- `LOCALITY_BROKER_PUBLIC_BASE_URL`: HTTPS public origin for the broker, for
  example `https://oauth.locality.example`. The broker uses this value to build
  provider callback URLs returned as `provider_redirect_uri`. `/start` endpoints
  fail with `broker_config_error` until it is configured. See
  [`docs/deployment.md`](docs/deployment.md) for Cloudflare Workers setup.

## Required Secrets

- `LOCALITY_BROKER_SESSION_SECRET`: signs short-lived OAuth sessions.
- `LOCALITY_REFRESH_HANDLE_KEY`: encrypts opaque refresh handles in `handle` mode.
- `LOCALITY_NOTION_CLIENT_ID`: Notion OAuth client ID.
- `LOCALITY_NOTION_CLIENT_SECRET`: Notion OAuth client secret.
- `LOCALITY_GOOGLE_CLIENT_ID`: Google OAuth client ID shared by Google Docs, Google Calendar, and Gmail.
- `LOCALITY_GOOGLE_CLIENT_SECRET`: Google OAuth client secret shared by Google Docs, Google Calendar, and Gmail.
- `LOCALITY_SLACK_CLIENT_ID`: Slack OAuth client ID.
- `LOCALITY_SLACK_CLIENT_SECRET`: Slack OAuth client secret.

Optional connector overrides:

- `LOCALITY_NOTION_REDIRECT_URIS`, `LOCALITY_GOOGLE_DOCS_REDIRECT_URIS`, `LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS`, `LOCALITY_GMAIL_REDIRECT_URIS`, `LOCALITY_SLACK_REDIRECT_URIS`: comma-separated allowed loopback redirect URIs.
- `LOCALITY_NOTION_AUTH_BASE_URL`, `LOCALITY_GOOGLE_DOCS_AUTH_BASE_URL`, `LOCALITY_GOOGLE_CALENDAR_AUTH_BASE_URL`, `LOCALITY_GMAIL_AUTH_BASE_URL`, `LOCALITY_SLACK_AUTH_BASE_URL`: provider authorization base URL.
- `LOCALITY_NOTION_API_BASE_URL`, `LOCALITY_GOOGLE_DOCS_API_BASE_URL`, `LOCALITY_GOOGLE_CALENDAR_API_BASE_URL`, `LOCALITY_GMAIL_API_BASE_URL`, `LOCALITY_SLACK_API_BASE_URL`: provider token API base URL.

## Deployment

Recommended first deployment target: Cloudflare Workers.

This service is TypeScript-native, latency-insensitive, and only needs provider
secrets, outbound HTTPS, and the configured Slack refresh Durable Object.
Workers fit that shape well. Use
`wrangler secret put` for secrets, keep only non-sensitive defaults in
`wrangler.toml`, and let
`.github/workflows/oauth-service-deploy.yml` deploy on `main` once
`CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID` are configured. See
`docs/deployment.md` for the full CD setup.

Alternatives:

- Vercel Functions: good if the rest of the web stack already lives on Vercel.
- Fly.io: good if we later need a long-running service, regional control, or a
  stateful companion process.

Cloudflare Workers is the smallest operational surface for this broker.
