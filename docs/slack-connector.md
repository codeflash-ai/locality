# Slack Connector

The Slack connector is the first-party source id `slack`. V1 mounts Slack
conversation history as read-only Markdown so agents and editors can inspect
recent team context without gaining write access to Slack.

## Setup

```bash
loc connect slack
loc mount slack ~/Locality/slack-main
```

Locality requests Slack's `channels:join` scope. Mounts whose `--types` include
`public_channel` join public channels before reading history. This mutates
Slack membership for the connected app. Private channels still require an
explicit Slack invite.

The default Slack connector settings are:

```json
{"slack":{"history_limit":15,"types":["public_channel","private_channel","im","mpim"],"auto_join_public_channels":true}}
```

## OAuth scopes

Locality requests bot scopes for channel metadata and history, public channel
joining, users and team metadata, and file metadata. It does not request
`chat:write`, admin scopes, search scopes, or user email scope.

## Filesystem contract

```text
slack-main/
  channels/
    product-C123/
      recent.md
  private-channels/
    leadership-G123/
      recent.md
  dms/
    jane-doe-D123/
      recent.md
  group-dms/
    design-triage-G456/
      recent.md
  users.md
```

- `channels/` contains public channels whose history is readable by the
  connected app. Mounts whose types include `public_channel` attempt to join
  public channels automatically before reading history.
- `private-channels/` contains private channels visible to the connected bot.
- `dms/` contains direct message conversations visible to the connected bot.
- `group-dms/` contains multi-person direct message conversations visible to
  the connected bot.
- Conversation directory names include the Slack conversation id suffix for
  stable disambiguation.
- `users.md` contains workspace user metadata.
- Each conversation directory contains `recent.md` with the latest projected
  messages for that conversation. Parent messages with Slack thread replies
  include a bounded inline `Thread` section with the fetched reply messages.

## Sync and limits

Slack uses separate connector-owned quota scopes for metadata, conversation
history, and thread replies. Metadata calls cover conversation and user
listings. History calls cover `conversations.history`; thread reply calls cover
bounded `conversations.replies` expansion for threaded parent messages.

Locality defaults to `history_limit: 15`, a 1 request/minute history gate, and
a bounded one-at-a-time reply expansion scope. That default follows Slack's
strictest documented history page size while keeping FUSE reads bounded enough
to open threaded conversations. Marketplace apps and internal customer-built
apps may have different provider limits, but Locality still treats Slack 429
responses as provider cooldowns.

Freshness checks use the bounded conversation history and user metadata payload.
Thread reply bodies are expanded when `recent.md` hydrates, so reply-only edits
become visible on the next hydration or explicit pull without making background
freshness block on `conversations.replies`.

## Write policy

Slack mounts are read-only. Locality rejects edits, creates, renames, moves,
deletes, push writes, undo writes, and autosave writes under Slack mounts.

V1 does not post messages, subscribe to Slack events, or store arbitrary Slack
search results.

## Useful commands

```bash
loc connect slack
loc mount slack ~/Locality/slack-main
loc status ~/Locality/slack-main
loc diff ~/Locality/slack-main
loc pull ~/Locality/slack-main
```
