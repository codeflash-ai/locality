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

Pull enumerates the recent 100 inbox messages and recent 100 sent messages for
v1. The `draft/` folder is created locally, but the connector does not enumerate
remote Gmail drafts in v1.

Inbox and sent messages render as Markdown with Locality identity frontmatter and
Gmail metadata frontmatter such as mailbox, message ID, thread ID, labels,
sender, recipients, subject, and date. The connector renders available plain text
body content, or strips HTML tags as a fallback. When a specific message is
hydrated, inbound attachments are downloaded on demand under
`.loc/gmail/attachments/...` and the hydrated message frontmatter records their
local paths. Metadata-only stubs omit attachment frontmatter because attachment
presence is unknown until full message hydration.

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

`loc push` for a Gmail draft creates a Gmail draft and immediately sends it.
Push is the send action. Attachments are not supported for Gmail draft sends in
v1; `attachment` or `attachments` frontmatter is rejected.

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

Review and send a draft:

```bash
./target/debug/loc status "$HOME/Locality/gmail-main/draft/reply.md"
./target/debug/loc diff "$HOME/Locality/gmail-main/draft/reply.md"
./target/debug/loc push "$HOME/Locality/gmail-main/draft/reply.md"
```
