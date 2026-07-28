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

`inbox/` and `sent/` are read-only. `draft/` mirrors Gmail drafts and is the
local write surface for unsent draft creation and edits.

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

Gmail and Google Docs use the same broker-configured Google OAuth client:
`LOCALITY_GOOGLE_CLIENT_ID` and `LOCALITY_GOOGLE_CLIENT_SECRET`. Register both
the Gmail and Google Docs localhost callbacks on that Google OAuth client.

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

By default, Pull enumerates the recent 100 inbox threads, recent 100 sent
threads, and recent 100 Gmail drafts. The `draft/` folder contains both drafts
created in Gmail and local changes to those drafts. `loc push` creates or updates
an unsent Gmail UI draft; it never sends mail.

Gmail mounts can be registered with a date window:

```bash
./target/debug/loc mount gmail ~/Locality/gmail-main \
  --after 2026-07-01 \
  --before 2026-07-15
```

Date-window mounts use Gmail search query dates and page through all matching
messages for `inbox/` and `sent/` instead of stopping after the first recent 100
results.

Message view is available as an explicit compatibility projection:

```text
gmail-main/
  inbox/
    quarterly-update_msg-1.md
  sent/
    reply_msg-2.md
  draft/
```

Thread view is the default:

```bash
./target/debug/loc mount gmail ~/Locality/gmail-main
```

Thread view projects thread pages and child messages:

```text
gmail-main/
  inbox/
    quarterly-update_thread-a/
      page.md
      quarterly-update_msg-1.md
  sent/
  draft/
```

New mounts persist Gmail projection layout version `2` together with the
explicit `threads` view. A mount created by an older Locality version with
implicit `{}` settings used the old flat-message default; Locality refuses to
reinterpret that mount in place because doing so would leave old message files
beside new thread directories. Preserve that mount by registering it explicitly
with `--view messages`, or create a new mount ID and root for thread view after
reviewing any local work in the old mount.

Inbox and sent content is read-only. Draft files are editable: creating a
Markdown file directly under `draft/` creates an unsent Gmail draft when pushed,
and editing a projected draft updates it. Locality has no send endpoint; send
the completed draft from the Gmail UI.

To reply in an existing thread, create the draft from its hydrated thread
directory. Locality uses the latest child message by default, or the explicitly
selected message, and carries the Gmail thread ID plus RFC reply headers into
the draft:

```bash
./target/debug/loc create gmail-reply \
  "$HOME/Locality/gmail-main/inbox/quarterly-update_thread-a"

./target/debug/loc create gmail-reply \
  "$HOME/Locality/gmail-main/inbox/quarterly-update_thread-a" \
  --message quarterly-update_msg-1.md
```

The selected message must be hydrated, because Locality needs its RFC
`Message-ID` metadata to produce a correctly threaded reply. The command writes
a new file directly under `draft/`; review it with `loc diff` before pushing.

## Attachments

Gmail attachment bytes are fetched on demand. Enumeration and metadata refreshes
do not download attachment bodies. When a specific message or thread is
hydrated, Locality downloads the attachment bodies referenced by that message or
thread and writes them under:

```text
.loc/gmail/attachments/<message-id>/
```

Rendered message frontmatter includes attachment filename, MIME type, size,
Gmail attachment ID, and the local path. Draft creation rejects `attachment` or
`attachments` frontmatter. To avoid rewriting content Locality cannot preserve,
V1 updates are limited to simple `text/plain` drafts with no attachments,
multipart/HTML content, or custom MIME headers. Edit other drafts in Gmail.

## Write Policy

`inbox/` and `sent/` are read-only. File Provider and source write policy should
reject edits and deletes there.

Creating or editing a Markdown file directly under `draft/` is writable:

```text
draft/reply.md
```

Nested draft files are rejected, and draft deletion is unsupported:

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

`loc push` for a Gmail draft creates a new unsent Gmail draft or updates an
existing simple text-only draft. Locality has no send endpoint: send it from the
Gmail UI after review. Attachments are not supported for Gmail draft creation or
updates in v1; `attachment` or `attachments` frontmatter is rejected. HTML,
multipart, or custom-MIME drafts must be edited in Gmail so their content is not
lost.

On macOS File Provider mounts, the push journal remembers the temporary local
draft identifier before sending. Once Gmail apply and read-back both succeed,
Locality removes that exact File Provider item and signals both the `draft/` and
`sent/` containers. Remote or unconfirmed item deletion remains blocked. This
does not require the user to run `loc pull` or refresh Finder.

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

Review and create or update a Gmail UI draft:

```bash
./target/debug/loc status "$HOME/Locality/gmail-main/draft/reply.md"
./target/debug/loc diff "$HOME/Locality/gmail-main/draft/reply.md"
./target/debug/loc push "$HOME/Locality/gmail-main/draft/reply.md"
```
