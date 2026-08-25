# Hosted Google Docs Picker Design

## Goal

Replace Desktop's loopback Google Picker page with a hosted HTTPS Picker page
at the Locality OAuth broker. The flow must retain the narrow Google Docs and
`drive.file` scopes, keep the broker stateless, and ensure Desktop never
receives or persists a Picker API key or Google OAuth access token.

## Constraints

- Google Picker requires an HTTP(S) parent origin. It cannot run under the
  packaged Tauri `tauri://` origin.
- `drive.file` does not authorize Locality to enumerate a user's Drive, so a
  native Locality Drive browser is out of scope.
- The Picker API key, Picker project number, and Google OAuth client ID must
  belong to the same Google Cloud project.
- The OAuth broker remains stateless: no Durable Object, KV, database, token,
  selection, or session record is introduced.
- The hosted Picker JavaScript necessarily receives a short-lived access token
  because Google Picker requires `setOAuthToken`. It must not persist, log, or
  return that token to Desktop.

## Architecture

The existing OAuth broker issues encrypted and signed, short-lived capability
tokens. A capability contains all state needed for the next step, so the broker
does not retain a session record.

```text
Desktop                         OAuth broker HTTPS                 Google Picker
   | create session + secret           |                                  |
   |---------------------------------->| decrypt refresh handle            |
   |<-- browser capability URL --------|                                  |
   | open browser                                                         |
   |                                   |<----- page request --------------|
   |                                   | refresh Google access token       |
   |                                   |------ hosted Picker page -------->|
   |                                   |                                  | select Docs
   |                                   |<------ selected IDs -------------|
   |<------ locality:// completion ---|                                  |
   | redeem completion + secret        |                                  |
   |---------------------------------->| verify secret binding             |
   |<------ selected IDs --------------|                                  |
   | create local mount                |                                  |
```

## Capability flow

### 1. Create picker session

Desktop obtains the active Google Docs credential from the OS credential
store. It generates a random 32-byte redemption secret in memory, then calls
the broker with the credential's opaque `refresh_token_handle` and the SHA-256
hash of that secret.

The broker validates and decrypts the refresh handle, then returns an opaque
browser capability URL. The URL capability is encrypted and authenticated with
the broker secret and expires after ten minutes. It contains:

- version and expiry;
- connector `google-docs`;
- refresh handle (encrypted inside the capability);
- the redemption-secret hash;
- a random capability identifier for Desktop-side replay tracking.

Desktop opens this HTTPS URL in the default browser. It retains only the
redemption secret and capability identifier in memory.

### 2. Serve hosted Picker

The broker decrypts and validates the browser capability on each request. It
uses the refresh handle only inside the broker to obtain a fresh Google access
token. It renders a no-store hosted page configured with:

- the broker project's Picker API key;
- the same project's numeric Picker app ID;
- the short-lived access token;
- the opaque browser capability.

The page loads Google Picker with Docs-only multi-select and `drive.file`
access. It does not expose the OAuth client secret, refresh handle, or Desktop
redemption secret. It disables submission after the first Select action and
shows a visible failure message for any broker error.

### 3. Complete selection

The hosted page posts selected document IDs and the browser capability to the
broker. The broker validates and canonicalizes IDs with
`GoogleDocsMountSettings`, then creates an encrypted completion capability.
The completion capability contains the canonical IDs, redemption-secret hash,
expiry, and capability identifier. The broker returns a `303` redirect to:

```text
locality://google-docs-picker?completion=<opaque-capability>
```

No document ID or OAuth material appears in the URL outside the encrypted
completion capability.

### 4. Redeem in Desktop

Desktop registers the `locality://` scheme through Tauri's deep-link support.
On receipt, it verifies the completion URL shape and sends the opaque
completion capability plus its in-memory redemption secret to the broker.
The broker decrypts the capability, compares the secret hash in constant time,
checks expiry and connector, then returns canonical document IDs. Desktop
creates or reconfigures the mount using those IDs.

Desktop rejects a completion with an unknown, expired, or already locally
consumed capability identifier. It marks an identifier consumed before writing
mount state, so duplicate OS deep-link events cannot create duplicate mounts.

## Security and failure behavior

- Browser capability, completion capability, and Desktop redemption secret are
  independently random or authenticated. Possession of a browser URL does not
  permit completion redemption.
- The broker does not log access tokens, refresh handles, selected IDs, or
  opaque capabilities.
- Pages and broker responses use `Cache-Control: no-store` and
  `Referrer-Policy: no-referrer`.
- Picker API configuration is broker-only and validated at deployment: API
  key, app/project number, and OAuth client must be in the same Google Cloud
  project.
- Because the broker has no state, it cannot globally mark a completion token
  consumed. Completion capability expiry is short (five minutes); Desktop
  provides replay protection for its process lifetime. A fresh Desktop process
  can redeem a still-valid completion only if it also has the in-memory
  redemption secret, which it does not after restart.
- Broker or Google refresh errors leave the hosted page on a clear recoverable
  error state. Expired capabilities require starting selection again.
- If the Desktop app is unavailable for the deep link, the browser shows the
  completion page and offers the opaque completion token only as a diagnostic
  value; it must not display selected IDs or OAuth credentials.

## API surface

The OAuth service adds Google-Docs-only endpoints:

- `POST /v1/google-docs/picker/sessions` — accepts an opaque refresh handle
  and redemption-secret hash; returns a browser URL and expiry.
- `GET /v1/google-docs/picker/:capability` — serves the hosted Picker page.
- `POST /v1/google-docs/picker/:capability/selection` — validates selected
  IDs and redirects to the Locality deep link with completion capability.
- `POST /v1/google-docs/picker/redeem` — accepts completion capability and
  redemption secret; returns canonical document IDs.

All endpoint request and response schemas are versioned and must reject an
unknown version cleanly.

## Desktop changes

- Replace `choose_google_docs_in_browser`'s TCP listener with broker session
  creation, browser open, and deep-link await.
- Add a `locality://google-docs-picker` deep-link handler that only resolves a
  currently pending picker request.
- Keep the selected IDs in memory until the existing mount creation or
  reconfiguration command has completed.
- Delete the loopback page, HTTP parser, and Picker configuration from Desktop.

## Tests

- OAuth service tests cover capability encryption, expiry, connector binding,
  redemption-secret mismatch, malformed IDs, and no token/ID disclosure in
  page or error responses.
- Desktop tests cover session request construction, deep-link correlation,
  duplicate deep links, timeout, restart behavior, and existing mount creation
  with returned IDs.
- An end-to-end browser test verifies hosted Picker completion with a test
  callback payload and confirms the desktop-side mount request gets canonical
  IDs.
- Deployment validation tests require a numeric Picker app ID matching the
  Google OAuth client project number and a configured broker Picker API key.
