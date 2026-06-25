# Google Docs Connector Design

## Summary

Add Google Docs as a first-party Locality connector with connector id
`google-docs`. The connector should be product-complete for the normal Locality
workflow from the start: broker-backed OAuth, mount, pull, status, diff, push,
Live Mode compatibility, conflict safety, journals, and durable local state.

The connector uses Google Drive API for tree and file management, and Google
Docs API for document body reads and writes. Locality should not use a generated
Rust Google client; it will call the REST APIs directly through focused
transport code in a new connector crate.

## Goals

- Add `loc connect google-docs` using the same broker-backed OAuth model as
  `loc connect notion`.
- Add `loc mount google-docs` for a My Drive or Drive folder root.
- Store Google connection metadata in the existing connector-neutral
  `ConnectionRecord` and `ConnectorProfileRecord` tables.
- Store only local credential material and opaque broker refresh handles in the
  credential store. Do not store Google OAuth client secrets or raw Google
  refresh tokens locally.
- Reuse a connector-neutral OAuth broker client interface so Notion,
  Google Docs, and future connectors share the same local OAuth orchestration.
- Implement a live-capable read/write Google Docs connector with the same host
  semantics as Notion: canonical Markdown, synced shadows, push planning,
  remote concurrency checks, reconciliation, undo where supported, and
  conservative unsupported-operation errors.
- Keep Notion behavior unchanged.

## Non-Goals

- Do not add a developer-only direct OAuth path as the default product path.
- Do not use a generated Google Rust client.
- Do not support Google Sheets, Slides, Forms, Gmail, Calendar, or arbitrary
  binary Drive file sync in this first connector.
- Do not silently flatten unsupported Google Docs structures during push.
  Unsupported or lossy structures must be preserved as directives or block push
  until round-trip support exists.

## Public Interface

New connector id:

```text
google-docs
```

New commands:

```bash
loc connect google-docs [--name <id>] [--no-browser] [--broker-url <url>] [--redirect-uri <uri>] [--json]
loc mount google-docs <path> (--my-drive | --drive-folder <folder-url-or-id>) [--connection <id>] [--mount-id <id>] [--projection plain-files|macos-file-provider|linux-fuse|windows-cloud-files] [--read-only] [--json]
```

Defaults:

```text
connection id: google-docs-default
profile id: google-docs-oauth-default
mount id: google-docs-main
callback URI: http://localhost:8757/oauth/google-docs/callback
broker env: LOCALITY_GOOGLE_DOCS_OAUTH_BROKER_URL
shared broker env fallback: LOCALITY_AUTH_BROKER_URL
```

The source descriptor registry should expose the connector display name
`Google Docs`, the default mount id, auth hints, and mount guidance. CLI,
daemon, desktop, MCP, and provider flows should consume that descriptor rather
than hard-coding Notion-specific assumptions.

## OAuth Broker Contract

Google Docs must use the same broker lifecycle shape as Notion:

```text
POST /v1/oauth/google-docs/start
GET  localhost callback handled by loc
POST /v1/oauth/google-docs/exchange
POST /v1/oauth/google-docs/refresh
```

The local client sends:

```json
{
  "connector": "google-docs",
  "redirect_uri": "http://localhost:8757/oauth/google-docs/callback"
}
```

The broker start response returns:

```json
{
  "connector": "google-docs",
  "client_id": "...",
  "authorization_url": "https://accounts.google.com/o/oauth2/v2/auth?...",
  "redirect_uri": "http://localhost:8757/oauth/google-docs/callback",
  "session": "...",
  "state": "...",
  "expires_in": 300
}
```

The local client opens the browser, listens on the redirect URI, validates
`state`, then exchanges:

```json
{
  "connector": "google-docs",
  "session": "...",
  "state": "...",
  "code": "...",
  "redirect_uri": "http://localhost:8757/oauth/google-docs/callback"
}
```

The broker returns a token payload:

```json
{
  "access_token": "...",
  "token_type": "Bearer",
  "expires_in": 3600,
  "refresh_token_handle": "...",
  "account_id": "...",
  "account_label": "user@example.com",
  "workspace_id": "google-drive",
  "workspace_name": "Google Drive",
  "scopes": [
    "openid",
    "email",
    "profile",
    "https://www.googleapis.com/auth/documents",
    "https://www.googleapis.com/auth/drive"
  ]
}
```

Refresh uses only the opaque handle:

```json
{
  "connector": "google-docs",
  "refresh_token_handle": "..."
}
```

The broker owns Google OAuth client credentials, authorization URL generation,
PKCE verifier storage, token exchange, raw refresh-token storage, refresh-token
rotation, and revocation policy. The local client owns browser launch,
localhost callback handling, state validation, connection record persistence,
and credential-store persistence of access-token plus refresh-handle bundles.

## OAuth Scopes

Request the following v1 scopes:

```text
openid
email
profile
https://www.googleapis.com/auth/documents
https://www.googleapis.com/auth/drive
```

`documents` is needed for Google Docs body read/write. Full `drive` is needed
because v1 is expected to be a complete Locality mount, including Drive
discovery and file management: list folders/docs, create Docs, rename, move,
trash/delete, and inspect file metadata. Narrower Drive scopes are not enough
for this product contract.

The broker project must enable Google Docs API and Google Drive API. The OAuth
consent screen must list exactly the scopes Locality requests, and the product
must be prepared for Google's sensitive/restricted-scope review requirements.

## Credential Shape

Add `StoredGoogleDocsCredential` in `locality-google-docs`:

```text
kind = "oauth"
access_token
token_type
oauth_client_id
oauth_broker_url
account_id
account_label
workspace_id
workspace_name
scopes
refresh_token_handle
acquired_at
expires_at
```

Do not include `oauth_client_secret` or raw `refresh_token`. This should mirror
Notion broker credentials as closely as possible and support handle rotation
when the broker returns a new handle during refresh.

## Connector Crate

Add `crates/locality-google-docs`.

Main modules:

- `oauth`: Google Docs credential payloads and shared broker integration glue.
- `client`: direct REST transport for Google Docs and Drive APIs.
- `drive_dto`: Drive file/folder DTOs used by enumeration and file mutations.
- `docs_dto`: Google Docs document DTOs used by fetch, render, parse, and write.
- `render`: Google Docs document model to canonical Markdown plus shadow.
- `parse`: canonical Markdown plus shadow back into planned Google Docs edits.
- `apply`: Drive file mutations and Docs `batchUpdate` execution.
- `connector`: `GoogleDocsConnector` implementing the connector trait.

Transport should be built around small traits for testability:

```text
GoogleDriveApi
GoogleDocsApi
GoogleDocsOAuthRefresh
```

The default implementations use `reqwest` blocking clients, consistent with the
Notion connector style.

## Projection Model

Use a Notion-compatible document directory projection for v1:

```text
Drive folder        -> local directory
Google Docs file    -> local directory containing page.md
```

Example:

```text
Marketing/
  Launch Brief/
    page.md
```

This intentionally favors consistency with the existing page-like Locality
workflow and avoids introducing a second editable-document projection shape
while the connector boundary is still being generalized. The containing
directory determines the Drive parent. Creating a new `page.md` under a local
directory creates a new Google Docs file under that Drive folder.

Frontmatter should use the connector-neutral Locality fields:

```yaml
---
loc:
  id: <google-doc-file-id>
  type: page
  connector: google-docs
  synced_at: <remote-version>
  remote_edited_at: <remote-version>
title: "Launch Brief"
---
```

The connector may store additional Google-specific metadata in shadow/native
state, not in user-edited frontmatter unless needed for stable identity or
debugging.

## Remote Tree And Versions

Drive enumeration provides the remote tree:

- file id
- MIME type
- name/title
- parent ids
- trashed/deleted state
- modified time
- Drive version or revision metadata when available
- permissions/capabilities needed for write preflight

Google Docs fetch provides the body and Docs revision id. Locality should treat
the connector remote version as an opaque token composed from Drive modified
time/version plus Docs revision id when both are available. Core compares this
only for equality.

## Rendering And Parsing

The first complete Google Docs renderer should support common structures:

- paragraphs
- headings
- bold, italic, underline, strikethrough, code-like text where representable
- links
- bulleted and numbered lists
- checklists if exposed in the Docs model
- tables
- inline images as preserved directives unless media download/upload support is
  implemented for Drive assets in the same slice
- page breaks, headers/footers, footnotes, equations, drawings, positioned
  objects, suggestions, and other lossy structures as `::loc{...}` directives

Parsing should support updating safe Markdown structures back into Docs
operations. Unsupported directives preserve remote identity and block unsafe
edits. The initial write path may replace a document body through a conservative
delete/insert plan only when the shadow proves that no unsupported preserved
content would be lost. More granular range-based edits can follow after the
correctness path is in place.

## Push And Pull Semantics

Pull:

- Enumerate Drive tree under the configured root.
- Fetch Google Docs documents on hydration.
- Render canonical Markdown and shadow.
- Write local files through existing projection/hydration machinery.
- Re-download or repair missing connector-owned local artifacts if media
  support is added.

Diff/status:

- Reuse existing canonical shadow comparison and planner where possible.
- Surface Google-specific unsupported operations with connector-specific codes.

Push:

- Refresh OAuth token before remote calls when needed.
- Observe Drive/Docs remote version immediately before mutation.
- Refuse push if remote changed since the synced shadow unless the planner has
  an explicit safe merge.
- Apply document body edits with Docs `documents.batchUpdate`.
- Use Docs `WriteControl.requiredRevisionId` for strict concurrency when
  applying body changes.
- Apply title, move, create, and trash/delete through Drive `files` methods.
- Re-fetch changed or created documents after apply and reconcile the local
  shadow from the remote product state.

Undo:

- Record enough journal effects for supported body edits and Drive file
  mutations.
- Implement undo for operations that can be reversed safely.
- Return an unsupported undo error for operations without a safe reverse path.

## Error Handling

Use stable Locality error codes:

- `missing_connection`
- `auth_required`
- `connection_revoked`
- `auth_profile_unavailable`
- `oauth_broker_start_failed`
- `oauth_exchange_failed`
- `oauth_refresh_failed`
- `google_docs_rate_limited`
- `google_docs_permission_denied`
- `google_docs_remote_changed`
- `google_docs_unsupported_document_structure`
- `google_docs_unsupported_push_operation`

Errors should include suggested commands when actionable, for example:

```text
loc connect google-docs
loc pull <path>
loc diff <path>
```

## State And Compatibility

No SQLite physical schema change should be required for the connection/profile
records. If new durable semantic formats are introduced, such as Google Docs
native bundles or connector-specific shadow metadata, add component versioning
and repair paths.

Existing Notion state must continue to open and behave unchanged. Unknown
connector strings should still get generic descriptor guidance but must fail
remote I/O unless registered.

## Testing

Unit tests:

- shared OAuth broker start/exchange/refresh DTOs
- `loc connect google-docs` argument parsing
- broker OAuth stores refresh handles without secrets
- token refresh rotates access token and refresh handle
- Drive file listing/path projection
- Drive create/rename/move/trash request construction
- Docs fetch DTO decoding
- Docs render canonical Markdown
- Docs parse/apply plans for safe body edits
- unsupported structures preserve directives and block lossy push
- source registry exposes `google-docs`

Integration tests with fake APIs:

- connect, mount, pull, edit, diff, push, reconcile
- remote drift blocks push
- local create creates a Google Docs file under the containing Drive folder
- local rename/move maps to Drive file update
- delete/archive maps to Drive trash behavior
- expired access token refreshes through broker handle

Live tests:

- ignored by default
- use a disposable Google account/folder
- require broker credentials or a test broker endpoint
- verify through Google APIs after Locality push
- clean up scratch Drive files and folders

CI must continue running all non-live tests without Google credentials.

## References

- Google Docs API documents resource:
  https://developers.google.com/workspace/docs/api/reference/rest/v1/documents
- Google Docs `documents.get`:
  https://developers.google.com/workspace/docs/api/reference/rest/v1/documents/get
- Google Docs `documents.batchUpdate`:
  https://developers.google.com/workspace/docs/api/reference/rest/v1/documents/batchUpdate
- Google Docs scopes:
  https://developers.google.com/workspace/docs/api/auth
- Google Drive files resource:
  https://developers.google.com/workspace/drive/api/reference/rest/v3/files
- Google Drive scopes:
  https://developers.google.com/workspace/drive/api/guides/api-specific-auth
- Google OAuth installed app guidance:
  https://developers.google.com/identity/protocols/oauth2/native-app
- Google OAuth web server/offline access guidance:
  https://developers.google.com/identity/protocols/oauth2/web-server
