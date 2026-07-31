# Gmail Connector Summary

This document summarizes the first Gmail connector implementation.

## Connector Scope

Gmail is registered as a first-party Locality source connector named `gmail`.
It uses the normal Locality connection, mount, pull, hydrate, diff, push, status,
and live projection paths.

The connector projects a fixed mailbox shape:

```text
~/Locality/
  gmail-main/
    inbox/
    sent/
    draft/
```

`inbox/` and `sent/` are read-only. `draft/` is the local write surface for
outbound mail.

## OAuth

The Gmail OAuth flow uses the Locality OAuth broker endpoints:

- `/v1/oauth/gmail/start`
- `/v1/oauth/gmail/exchange`
- `/v1/oauth/gmail/refresh`

The default callback is:

```text
http://localhost:8757/oauth/gmail/callback
```

The broker allowlist also supports:

```text
http://127.0.0.1:8757/oauth/gmail/callback
```

Gmail, Google Calendar, and Google Docs use the same broker-configured Google
OAuth client: `LOCALITY_GOOGLE_CLIENT_ID` and
`LOCALITY_GOOGLE_CLIENT_SECRET`. In production, the Google OAuth client must
register the hosted HTTPS callbacks used by the broker:

```text
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-docs/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/google-calendar/callback
https://afs-oauth-broker.saurabh-b07.workers.dev/v1/oauth/gmail/callback
```

The localhost callbacks remain broker loopback allowlists for the local handoff.
Developer-owned or direct OAuth setups may still register and use local callbacks
where applicable.

The default connection ID is `gmail-default`, the default mount ID is
`gmail-main`, and the default OAuth profile is `gmail-oauth-default`.

The broker requests these scopes:

- `openid`
- `email`
- `profile`
- `https://www.googleapis.com/auth/gmail.readonly`
- `https://www.googleapis.com/auth/gmail.compose`

No broader Gmail account scope is required for this connector.

CLI overrides:

- `LOCALITY_GMAIL_OAUTH_BROKER_URL`
- `LOCALITY_AUTH_BROKER_URL`
- `LOCALITY_GMAIL_OAUTH_REDIRECT_URI`

## Projection And Pull

By default, Pull enumerates the recent 100 inbox messages and recent 100 sent
messages and recent 100 Gmail drafts. The `draft/` folder is the local staging surface
for new Gmail drafts. When pushed, a local draft becomes an unsent Gmail draft
and is visible in the Gmail UI; drafts created in Gmail are pulled into this
folder too.

Gmail mounts can be registered with a date window:

```bash
./target/debug/loc mount gmail ~/Locality/gmail-main \
  --after 2026-07-01 \
  --before 2026-07-15
```

Date-window mounts use Gmail search query dates and page through all matching
messages for `inbox/` and `sent/` instead of stopping after the first recent 100
results.

Message view is the default projection:

```text
gmail-main/
  inbox/
    1720900000000-quarterly-update-msg-1.md
  sent/
    1720900100000-reply-msg-2.md
  draft/
```

Thread view is opt-in:

```bash
./target/debug/loc mount gmail ~/Locality/gmail-main --view threads
```

Thread view projects thread pages and child messages:

```text
gmail-main/
  inbox/
    1720900000000-quarterly-update-thread-a/
      page.md
      1720900000000-quarterly-update-msg-1.md
  sent/
  draft/
```

Inbox, sent, and thread content is read-only. Creating a Markdown file directly
under `draft/` creates an unsent Gmail draft when pushed.

## Attachments

Gmail attachment bytes are fetched on demand. Enumeration and metadata refreshes
do not download attachment bodies. When a specific message or thread is
hydrated, Locality downloads the attachment bodies referenced by that message or
thread and writes them under:

```text
.loc/gmail/attachments/<message-id>/
```

Rendered message frontmatter includes attachment filename, MIME type, size,
Gmail attachment ID, and the local path. Draft sends still reject `attachment`
or `attachments` frontmatter; outbound attachments require a separate design.

## Write Policy

`inbox/` and `sent/` are read-only. File Provider and source write policy should
reject edits and deletes there.

Creating a Markdown file directly under `draft/` is writable:

```text
draft/reply.md
```

Nested draft files are rejected:

```text
draft/replies/reply.md
```

Draft frontmatter requires `to` and either `subject` or `title`. `cc` and `bcc`
are optional. Recipients may be a scalar string or a list.

```markdown
---
to:
  - person@example.com
cc: teammate@example.com
subject: Follow up
---

Thanks for the notes. I will follow up here.
```

`loc push` for a Gmail draft creates an unsent Gmail draft. Send it from the
Gmail UI after review. Attachments are not supported for Gmail draft creation
in v1; `attachment` or `attachments` frontmatter is rejected.

On macOS File Provider mounts, the push journal remembers the temporary local
draft identifier before sending. Once Gmail apply and read-back both succeed,
Locality removes that exact File Provider item and signals both the `draft/` and
`sent/` containers. Remote or unconfirmed item deletion remains blocked. This
does not require the user to run `loc pull` or refresh Finder.

## Live E2E

`tests/live_gmail_vfs_roundtrip.sh` exercises the live Gmail API, CLI
mount/pull/diff/push, daemon, and Linux FUSE projection. It creates an unsent
Gmail draft through the mounted `draft/` folder, verifies the draft projection,
and deletes the Gmail draft through Gmail API cleanup.

Use a stored `connection:gmail-live` credential and a recipient address:

```bash
secret_ref='connection:gmail-live'
secret_hex="$(printf '%s' "$secret_ref" | od -An -tx1 -v | tr -d ' \n')"
export LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON="$(cat "$HOME/.loc/credentials/$secret_hex")"
export LOCALITY_GMAIL_LIVE_TO_EMAIL='you@example.com'
```

Use the full stored credential JSON. The live harness requires
`access_token`, `oauth_broker_url`, `refresh_token_handle`, and numeric
`expires_at` so it can exercise broker refresh when the token expires.

Run the gated live check:

```bash
LOCALITY_LIVE_GMAIL_VFS=1 tests/live_gmail_vfs_roundtrip.sh
```

## Useful Commands

Connect with the local broker:

```bash
./target/debug/loc connect gmail --name gmail-default --broker-url http://127.0.0.1:8787
```

Mount Gmail:

```bash
./target/debug/loc mount gmail ~/Locality/gmail-main --projection linux-fuse
```

Force enumeration:

```bash
./target/debug/loc pull --json "$HOME/Locality/gmail-main"
```

Review and create a Gmail UI draft:

```bash
./target/debug/loc status "$HOME/Locality/gmail-main/draft/reply.md"
./target/debug/loc diff "$HOME/Locality/gmail-main/draft/reply.md"
./target/debug/loc push "$HOME/Locality/gmail-main/draft/reply.md"
```
