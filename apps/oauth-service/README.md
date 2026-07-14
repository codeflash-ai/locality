# Locality Auth Broker

Minimal OAuth broker for Locality connector auth.

The broker exists for providers whose OAuth REST API requires a confidential
client secret. The local `loc` client keeps the normal desktop UX: start a
localhost callback, open the provider consent page, receive the authorization
code, and store returned credentials in the OS credential store. This service
only performs the confidential token exchange and refresh calls.

## Flow

```text
loc CLI -> broker /start
loc CLI <- authorization_url, state, signed session
loc CLI -> browser -> provider OAuth consent
provider -> localhost callback on the user's machine
loc CLI -> broker /exchange with code, state, session, redirect_uri
broker -> provider token endpoint with client_secret
broker -> loc CLI with access token and refresh handle
```

Refresh is similarly narrow:

```text
loc CLI -> broker /refresh with refresh_token_handle
broker -> provider token endpoint with client_secret
broker -> loc CLI with new access token and new refresh handle
```

The broker does not persist page content or tokens. In `handle` mode, it returns
an encrypted opaque refresh handle instead of the raw provider refresh token.

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
  "session": "signed-session",
  "state": "opaque-state",
  "expires_in": 600
}
```

### `POST /v1/oauth/notion/exchange`

Request:

```json
{
  "session": "signed-session",
  "state": "opaque-state",
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

Response:

```json
{
  "connector": "google-docs",
  "client_id": "public-client-id",
  "authorization_url": "https://accounts.google.com/o/oauth2/v2/auth?...",
  "redirect_uri": "http://localhost:8757/oauth/google-docs/callback",
  "session": "signed-session",
  "state": "opaque-state",
  "expires_in": 600
}
```

### `POST /v1/oauth/google-docs/exchange`

Request:

```json
{
  "session": "signed-session",
  "state": "opaque-state",
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
  "session": "signed-session",
  "state": "opaque-state",
  "expires_in": 600
}
```

### `POST /v1/oauth/gmail/exchange`

Request:

```json
{
  "session": "signed-session",
  "state": "opaque-state",
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

## Required Secrets

- `LOCALITY_BROKER_SESSION_SECRET`: signs short-lived OAuth sessions.
- `LOCALITY_REFRESH_HANDLE_KEY`: encrypts opaque refresh handles in `handle` mode.
- `LOCALITY_NOTION_CLIENT_ID`: Notion OAuth client ID.
- `LOCALITY_NOTION_CLIENT_SECRET`: Notion OAuth client secret.
- `LOCALITY_GOOGLE_CLIENT_ID`: Google OAuth client ID shared by Google Docs and Gmail.
- `LOCALITY_GOOGLE_CLIENT_SECRET`: Google OAuth client secret shared by Google Docs and Gmail.

Optional connector overrides:

- `LOCALITY_NOTION_REDIRECT_URIS`, `LOCALITY_GOOGLE_DOCS_REDIRECT_URIS`, `LOCALITY_GMAIL_REDIRECT_URIS`: comma-separated allowed loopback redirect URIs.
- `LOCALITY_NOTION_AUTH_BASE_URL`, `LOCALITY_GOOGLE_DOCS_AUTH_BASE_URL`, `LOCALITY_GMAIL_AUTH_BASE_URL`: provider authorization base URL.
- `LOCALITY_NOTION_API_BASE_URL`, `LOCALITY_GOOGLE_DOCS_API_BASE_URL`, `LOCALITY_GMAIL_API_BASE_URL`: provider token API base URL.

## Deployment

Recommended first deployment target: Cloudflare Workers.

This service is stateless, TypeScript-native, latency-insensitive, and only
needs provider secrets plus outbound HTTPS. Workers fit that shape well. Use
`wrangler secret put` for secrets, keep only non-sensitive defaults in
`wrangler.toml`, and deploy from GitHub Actions once the repository is pushed.

Alternatives:

- Vercel Functions: good if the rest of the web stack already lives on Vercel.
- Fly.io: good if we later need a long-running service, regional control, or a
  stateful companion process.

Cloudflare Workers is the smallest operational surface for this broker.
