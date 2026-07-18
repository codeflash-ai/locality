# Slack Connector

Locality's Slack connector is a first-party read-only source named `slack`.
V1 exposes public channels only. It does not post messages, create replies,
edit, delete, backfill entire workspaces, read private channels, read DMs, read
group DMs, read files, or traverse Enterprise Grid org-wide state.

## Setup

Connect through the Locality OAuth broker:

```bash
loc connect slack
loc mount slack ~/Library/CloudStorage/Locality/slack-main
```

Defaults:

- connection ID: `slack-default`
- mount ID: `slack-main`
- OAuth profile: `slack-oauth-default`
- callback: `http://localhost:8757/oauth/slack/callback`
- scopes: `channels:read`, `channels:history`, `users:read`

Broker endpoints:

- `/v1/oauth/slack/start`
- `/v1/oauth/slack/exchange`
- `/v1/oauth/slack/refresh`

Broker environment:

- `LOCALITY_SLACK_CLIENT_ID`
- `LOCALITY_SLACK_CLIENT_SECRET`
- `LOCALITY_SLACK_REDIRECT_URIS`
- optional test overrides: `LOCALITY_SLACK_AUTH_BASE_URL`,
  `LOCALITY_SLACK_API_BASE_URL`, `LOCALITY_SLACK_OAUTH_BROKER_URL`

## Filesystem Contract

The mount projects public-channel discovery and hydrates content lazily:

```text
slack-main/
  channels/
    general/
      recent.md
      threads/
        2026-07-17-15.22.10-1721239330.000100.md
```

Channel directories are stubs. `recent.md` fetches the latest channel messages
on read. Thread files are listed from recent messages that have replies and
hydrate only when opened.

The default recent history limit is 15 messages. A mount may lower that limit:

```bash
loc mount slack ~/Library/CloudStorage/Locality/slack-main --recent-limit 7
```

Settings persist as:

```json
{"recent_limit":7,"conversation_types":"public_channel"}
```

V1 rejects any other conversation type.

## Rendering

Rendered Markdown includes Locality identity frontmatter plus Slack metadata for
channel ID/name, content kind, Slack timestamps, and thread timestamp when
present. Message sections keep sender, UTC timestamp, message text, thread
reply count, permalink, and Slack message ID. Unsupported rich blocks render as
readable fallback text and a Locality unsupported directive rather than being
silently dropped.

## Write Policy

Slack mounts are read-only. Locality rejects create, edit, rename, move, delete,
push, and auto-save attempts under Slack mounts with:

```text
Slack connector is read-only in v1
```

Agents should inspect Slack files with `loc status` and `loc diff` when needed
but should not run `loc push` for Slack content.

## Sync And Limits

The connector uses Slack Web API methods:

- `auth.test`
- `conversations.list`
- `conversations.info`
- `conversations.history`
- `conversations.replies`
- `users.info`

Slack history and thread reads are intentionally lazy because recent Slack Web
API tiers may allow only one history or replies request per minute and return
up to 15 objects for affected app installations. Locality honors Slack
`Retry-After` responses and keeps V1 behavior conservative rather than trying
to backfill a workspace.
