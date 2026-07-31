# All OAuth Hosted Handoff Design

## Goal

Every Locality OAuth broker connector should use the same browser handoff model:
the OAuth provider redirects to the deployed TLS broker, the broker verifies a
signed local-handoff state, then the browser is redirected back to the local
loopback listener. The CLI and desktop flows continue to store OAuth credentials
locally.

## Scope

This extends the hosted handoff that currently exists for Notion to all OAuth
broker connectors:

- Notion: `/v1/oauth/notion/callback`
- Google Docs: `/v1/oauth/google-docs/callback`
- Google Calendar: `/v1/oauth/google-calendar/callback`
- Gmail: `/v1/oauth/gmail/callback`
- Slack: `/v1/oauth/slack/callback`

The internal/private backend remains out of scope. This change does not move
provider access tokens, refresh handles, or local credential state into a
backend service.

## Configuration

Loopback allowlists stay separate from hosted provider callbacks.

Existing `*_REDIRECT_URIS` variables remain local-loopback allowlists for the
callback URI where the CLI or desktop listener waits:

- `LOCALITY_NOTION_REDIRECT_URIS`
- `LOCALITY_GOOGLE_DOCS_REDIRECT_URIS`
- `LOCALITY_GOOGLE_CALENDAR_REDIRECT_URIS`
- `LOCALITY_GMAIL_REDIRECT_URIS`
- `LOCALITY_SLACK_REDIRECT_URIS`

New hosted callback variables configure the exact HTTPS callback URI registered
with each provider app:

- `LOCALITY_NOTION_HOSTED_CALLBACK_URI`
- `LOCALITY_GOOGLE_DOCS_HOSTED_CALLBACK_URI`
- `LOCALITY_GOOGLE_CALENDAR_HOSTED_CALLBACK_URI`
- `LOCALITY_GMAIL_HOSTED_CALLBACK_URI`
- `LOCALITY_SLACK_HOSTED_CALLBACK_URI`

Each hosted URI must be HTTPS, contain no userinfo, port, query, or fragment,
and match the connector callback path exactly.

## Broker API Behavior

Each connector start endpoint continues to accept a requested local loopback
`redirect_uri`. When the connector has no hosted callback configured, behavior
falls back to the current direct-loopback broker flow.

When the connector has a hosted callback configured, `/start` returns:

- `redirect_uri`: the validated local loopback callback where the client listens
- `authorization_redirect_uri`: the hosted HTTPS provider callback
- `exchange_redirect_uri`: the hosted HTTPS provider callback
- `state`: a signed local-handoff state payload
- `session`: a signed broker session bound to the same state and
  `exchange_redirect_uri`

The provider authorization URL uses `authorization_redirect_uri`.

Each connector gets a browser-facing `GET /v1/oauth/<connector>/callback`
route. It accepts provider `code` or `error` fields, verifies the signed
local-handoff state, checks that the state connector and provider callback match
the route/configuration, validates the local callback against the connector
loopback allowlist, and redirects with `303 See Other` to the local callback.

The callback route sets:

- `Cache-Control: no-store`
- `Referrer-Policy: no-referrer`

It does not persist provider codes, tokens, refresh handles, or local callback
URIs.

Each connector exchange endpoint accepts the connector hosted callback URI when
it matches configuration, otherwise accepts only the configured local loopback
URI. The signed session check still requires connector, state, and redirect URI
to match before any upstream provider exchange runs.

## Rust Client Behavior

The shared broker start response type should understand optional
`authorization_redirect_uri` and `exchange_redirect_uri` fields. The response
should expose helpers for:

- local listener redirect URI
- authorization redirect URI
- exchange redirect URI

Existing broker responses without those fields remain compatible and fall back
to `redirect_uri`.

Notion keeps its existing Notion-specific authorization URL normalization, but
the shared Google Docs, Google Calendar, Gmail, and Slack flows should use the
hosted authorization URL returned by the broker and pass the hosted
`exchange_redirect_uri` to the broker exchange when present.

## CLI And Desktop Behavior

For every broker-backed OAuth connector:

1. The client calls `/start` with the local loopback callback.
2. The browser opens the returned authorization URL.
3. The local listener waits on `redirect_uri`.
4. The hosted broker callback redirects the browser back to that local listener.
5. The client exchanges the code using `exchange_redirect_uri`.
6. The returned credential is stored in the local credential store.

This applies to CLI and desktop connection flows.

## Provider Registration

Production provider apps must register the hosted callback URI for their
connector paths on the deployed broker. The Google OAuth app must register the
Google Docs, Google Calendar, and Gmail hosted callback paths. Slack must
register the Slack hosted callback path. Notion must register the Notion hosted
callback path.

Local loopback callbacks remain relevant for local listener allowlists and for
developer-owned direct OAuth apps, but they are no longer the production public
app redirect URIs when hosted handoff is enabled.

## Error Handling And Security

Malformed, expired, unsigned, wrong-connector, wrong-provider-callback, and
unallowlisted-local-callback states are rejected before redirecting to any local
URI.

Hosted callback validation rejects explicit default ports such as `:443`.
Exchange requests with arbitrary redirect URIs are rejected before any upstream
provider token request.

Provider denial is redirected back to the local listener with `error`,
optional `error_description`, and `state`, and without `code`.

## Testing

OAuth service tests should cover each connector:

- hosted `/start` returns local, authorization, and exchange redirect fields
- authorization URL uses the hosted callback
- hosted `/callback` redirects successful provider codes to localhost
- hosted `/callback` redirects provider denial to localhost
- unsigned or wrong signed state is rejected
- exchange uses the hosted redirect URI upstream
- arbitrary exchange redirect URIs are rejected

Rust tests should cover:

- shared response fallback for older broker responses
- shared response hosted redirect helpers
- CLI connector exchange paths still store credentials locally when using hosted
  exchange redirect URIs

Verification should include the OAuth service suite, affected Rust OAuth/connect
tests, desktop check, docs checks, and a local Worker smoke test for at least two
connectors from different providers, including one Google connector.

## Non-Goals

- No private backend OAuth credential storage.
- No single shared `/v1/oauth/callback` route.
- No provider support beyond the existing OAuth broker connectors.
- No change to direct OAuth development flows except documentation clarity.
