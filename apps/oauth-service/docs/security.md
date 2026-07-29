# Security Model

## Goals

- Do not ship provider client secrets in the Locality CLI.
- Keep the local desktop OAuth UX seamless.
- Avoid broker-side persistence of user content or tokens in the initial design.
- Make the broker reusable for future confidential OAuth connectors.

## Non-Goals

- The broker is not a content relay.
- The broker is not an account system.
- The broker does not prove that the caller is the official Locality binary. Open
  source desktop clients cannot keep such an attestation secret.

## Secret Handling

The provider client secret lives only in the deployed broker environment. The
CLI may ship or learn the OAuth client ID because client IDs are public
identifiers, not secrets.

The broker supports two refresh modes:

- `handle`: default production mode. The broker encrypts the provider refresh
  token into an opaque handle. The CLI stores the handle in the OS credential
  store and sends it back to refresh. In this mode, raw refresh tokens are not
  accepted by the refresh endpoint.
- `raw`: development compatibility mode. The broker returns the provider refresh
  token directly. The CLI must still store it only in the OS credential store.

## Current Controls

- OAuth sessions are short-lived HMAC-signed payloads.
- Session verification checks state, connector, redirect URI, expiry, and payload
  shape before exchanging a code.
- Local callback URIs are restricted to configured allowlists before any browser
  handoff or token exchange.
- Production handle mode keeps provider refresh tokens inside encrypted opaque
  handles before returning them to local clients.
- Upstream OAuth error bodies are not returned to callers.

## Abuse Model

The broker intentionally exposes a narrow token-exchange API. Anyone can call
it, but a successful exchange still requires a valid provider authorization code
or refresh handle for the Locality OAuth app.

Deployment controls to add before public launch:

- provider-specific rate limits on `/exchange` and `/refresh`;
- structured logs that exclude request bodies and Authorization headers;
- alerting on OAuth error spikes;
- optional per-release client metadata so abuse can be segmented by Locality version;
- strict redirect URI allowlists.

## Redirects

The broker keeps two redirect boundaries separate for every OAuth connector:

- `LOCALITY_<CONNECTOR>_REDIRECT_URIS` is a loopback-only allowlist for local callbacks such as `http://localhost:8757/oauth/gmail/callback`.
- `LOCALITY_<CONNECTOR>_HOSTED_CALLBACK_URI` is one exact HTTPS callback served by this broker at the connector's `/v1/oauth/<connector>/callback` path.

When hosted handoff is enabled by both a configured hosted callback URI and a
start request with `hosted_callback_handoff: true`, the provider authorization
request uses the hosted callback URI. The callback route verifies a signed state
payload before redirecting to a loopback URI from the allowlist. The token
exchange also uses the hosted callback URI so the provider sees the same
redirect URI in both OAuth steps. Clients that do not opt in stay on the direct
loopback flow.
