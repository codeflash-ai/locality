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
    outbox/
```

`inbox/` and `sent/` are read-only message folders. `draft/` contains remote
Gmail drafts and local draft creates. Editing a remote draft and pushing updates
the Gmail draft. `outbox/` is local-only send staging for reviewed direct sends.
A Markdown file created directly under `outbox/` is sent through Gmail when
pushed.

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

Gmail, Google Docs, and Google Calendar use the same broker-configured Google
OAuth client: `LOCALITY_GOOGLE_CLIENT_ID` and `LOCALITY_GOOGLE_CLIENT_SECRET`.
Register all Google connector localhost callbacks on that Google OAuth client.

The default connection ID is `gmail-default`, the default mount ID is
`gmail-main`, and the default OAuth profile is `gmail-oauth-default`.

The broker requests these scopes:

- `openid`
- `email`
- `profile`
- `https://www.googleapis.com/auth/gmail.readonly`
- `https://www.googleapis.com/auth/gmail.compose`

No broader Gmail account scope is required for this connector.

## Google OAuth Verification

Use `connectors/oauth-verification/gmail.json` as the source of truth when
configuring the Google Cloud Console and preparing the verification demo. The
runtime OAuth request includes the identity scopes above, but the Gmail API
scopes submitted for verification must be exactly:

- `https://www.googleapis.com/auth/gmail.readonly`
- `https://www.googleapis.com/auth/gmail.compose`

Do not submit broader or unused Gmail scopes for this connector:

- `https://mail.google.com/`
- `https://www.googleapis.com/auth/gmail.modify`
- `https://www.googleapis.com/auth/gmail.drafts.create`
- `https://www.googleapis.com/auth/gmail.drafts.readonly`
- `https://www.googleapis.com/auth/gmail.metadata`
- `https://www.googleapis.com/auth/gmail.insert`
- `https://www.googleapis.com/auth/gmail.addons.current.message.metadata`
- `https://www.googleapis.com/auth/gmail.addons.current.message.readonly`
- `https://www.googleapis.com/auth/gmail.send`

The verification video should show the consent screen with all requested
permissions expanded and readable, then demonstrate the maximum user-facing
extent of those two Gmail API scopes:

1. Connect and mount Gmail with `loc connect gmail`, `loc mount gmail`, and
   `loc pull`.
2. Open projected inbox or sent Markdown to show Locality reads full message
   content. This is why `gmail.metadata` is insufficient.
3. Open a message with an attachment and show the hydrated local attachment
   file created from Gmail read access.
4. If thread view is part of the submitted app surface, mount with
   `--view threads` and open a thread plus a child message.
5. Create a Markdown file directly under `draft/`, push it after review, and
   show the matching unsent draft in the user's Gmail account.
6. Edit an existing remote Gmail draft under `draft/`, push it after review,
   and show the updated unsent draft in the user's Gmail account.
7. Create a Markdown file directly under `outbox/` or move a remote draft from
   `draft/` to `outbox/`, push it after review, and show the matching message
   in Gmail Sent.

CLI overrides:

- `LOCALITY_GMAIL_OAUTH_BROKER_URL`
- `LOCALITY_AUTH_BROKER_URL`
- `LOCALITY_GMAIL_OAUTH_REDIRECT_URI`

## Projection And Pull

By default, Pull enumerates the recent 100 inbox messages, recent 100 sent
messages, and recent 100 Gmail drafts. The `draft/` folder contains remote
Gmail drafts and local draft creates: drafts created in Gmail are pulled there,
editing a remote draft and pushing updates the Gmail draft, and a local Markdown
file pushed from `draft/` becomes another unsent Gmail draft. The `outbox/`
folder is local-only outbound staging for reviewed direct sends; pushing a
direct child under `outbox/` sends the message and reconciles the result under
`sent/`.

Gmail mounts can be registered with a date window:

```bash
./target/debug/loc mount gmail ~/Locality/gmail-main \
  --after 2026-07-01 \
  --before 2026-07-15
```

Date-window mounts use Gmail search query dates and page through all matching
messages for `inbox/`, `sent/`, and `draft/` instead of stopping after the
first recent 100 results. `outbox/` is not pulled from remote history; it remains
local outbound staging for reviewed direct sends.

Message view is the default projection:

```text
gmail-main/
  inbox/
    1720900000000-quarterly-update-msg-1.md
  sent/
    1720900100000-reply-msg-2.md
  draft/
    1720900200000-unsent-follow-up-draft-msg-3.md
  outbox/
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
  outbox/
```

Inbox, sent, and thread content is read-only. Creating a Markdown file directly
under `draft/` creates an unsent Gmail draft when pushed. Editing a remote draft
under `draft/` and pushing updates that Gmail draft. Creating a Markdown file
directly under `outbox/` sends the message when pushed.

## Attachments

Gmail attachment bytes are fetched on demand. Enumeration and metadata refreshes
do not download attachment bodies. When a specific message or thread is
hydrated, Locality downloads the attachment bodies referenced by that message or
thread and writes them under:

```text
.loc/gmail/attachments/<message-id>/
```

Rendered message frontmatter includes attachment filename, MIME type, size,
Gmail attachment ID, and the local path. Gmail outbound attachments remain
unsupported; draft updates, draft creation, and direct send reject `attachment`
or `attachments` frontmatter.

## Write Policy

`inbox/` and `sent/` are read-only. File Provider and source write policy should
reject edits, creates, and deletes there:

```text
inbox/reply.md
sent/follow-up.md
```

Creating a Markdown file directly under `draft/` creates an unsent Gmail draft:

```text
draft/reply.md
```

Creating a Markdown file directly under `outbox/` is a reviewed direct send:

```text
outbox/reply.md
```

Moving an existing remote draft directly from `draft/` to `outbox/` sends that
draft after applying local edits:

```text
draft/follow-up.md -> outbox/follow-up.md
```

Nested outbound paths are rejected for both draft creation and direct send:

```text
draft/replies/reply.md
outbox/replies/reply.md
```

Outbound frontmatter for both `draft/` and `outbox/` requires `to` and either
`subject` or `title`. `cc` and `bcc` are optional. Recipients may be a scalar
string or a list.

```markdown
---
to:
  - person@example.com
cc: teammate@example.com
subject: Follow up
---

Thanks for the notes. I will follow up here.
```

`loc push` for a new Gmail file under `draft/` creates an unsent Gmail draft.
`loc push` for an existing remote draft under `draft/` updates the Gmail draft.
Move an existing remote draft from `draft/` to `outbox/` and push to send the
updated draft. `loc push` for a new Gmail file under `outbox/` directly sends
the message after Locality review and push approval. Gmail outbound attachments
are not supported in v1; `attachment` or `attachments` frontmatter is rejected
for both paths.

On macOS File Provider mounts, the push journal remembers the temporary local
`outbox/` item identifier before sending. Once Gmail apply and read-back both
succeed, Locality removes that exact File Provider item and signals both the
`outbox/` and `sent/` containers. Remote or unconfirmed item deletion remains
blocked. Pushing from `draft/` creates or updates an unsent Gmail draft and
signals the `draft/` container. This does not require the user to run `loc pull`
or refresh Finder.

## Live E2E

`tests/live_gmail_workflow_scenario.sh` runs the full local live Gmail workflow
scenario against the Gmail API, CLI mount/pull/diff/push, daemon, and Linux FUSE
projection. It pulls the mounted mailbox projection, creates an unsent Gmail
draft through `draft/`, verifies the draft projection, updates a pulled remote
draft, sends a Markdown file directly from `outbox/`, moves the edited remote
draft to `outbox/` to send it, verifies the sent messages, and cleans up scratch
drafts/messages where the granted Gmail scope allows it.

`tests/live_gmail_vfs_roundtrip.sh` is the underlying granular harness. With
`LOCALITY_LIVE_GMAIL_SEND=1`, it runs the same send, stale-draft prune, and
remote draft edit/send checks; without that flag, it stops after the safer draft
create/read/delete path.

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

Run the full local workflow scenario. This sends real email to
`LOCALITY_GMAIL_LIVE_TO_EMAIL`; use a scratch account or a recipient you control.

```bash
LOCALITY_LIVE_GMAIL_SCENARIO=1 tests/run_linux_fuse_ci.sh tests/live_gmail_workflow_scenario.sh
```

On a Linux machine with FUSE and build dependencies already installed, the
scenario can also be run directly:

```bash
LOCALITY_LIVE_GMAIL_SCENARIO=1 tests/live_gmail_workflow_scenario.sh
```

Run only the safer draft create/read/delete check:

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

Review and directly send a Gmail message:

```bash
./target/debug/loc status "$HOME/Locality/gmail-main/outbox/reply.md"
./target/debug/loc diff "$HOME/Locality/gmail-main/outbox/reply.md"
./target/debug/loc push "$HOME/Locality/gmail-main/outbox/reply.md"
```
