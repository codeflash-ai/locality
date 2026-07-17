# Slack Connector

The Slack connector is the first-party source id `slack`. V1 mounts Slack
conversation history as read-only Markdown so agents and editors can inspect
recent team context without gaining write access to Slack.

## Setup

```bash
loc connect slack
loc mount slack ~/Locality/slack-main
```

The default Slack connector settings are:

```json
{"slack":{"history_limit":15,"types":["public_channel","private_channel","im","mpim"]}}
```

## OAuth scopes

Locality requests read-only bot scopes for channel metadata and history, users
and team metadata, and file metadata. It does not request `chat:write`, admin
scopes, search scopes, or user email scope.

## Filesystem contract

```text
slack-main/
  channels/
    product/
      recent.md
  private-channels/
    leadership/
      recent.md
  dms/
    jane-doe/
      recent.md
  group-dms/
    design-triage/
      recent.md
  users.md
```

- `channels/` contains public channels.
- `private-channels/` contains private channels visible to the connected bot.
- `dms/` contains direct message conversations visible to the connected bot.
- `group-dms/` contains multi-person direct message conversations visible to
  the connected bot.
- `users.md` contains workspace user metadata.
- Each conversation directory contains `recent.md` with the latest projected
  messages for that conversation.

## Sync and limits

Slack uses separate connector-owned quota scopes for metadata and history.
Metadata calls cover conversation and user listings. History calls cover
`conversations.history` and related history fetches.

Locality defaults to `history_limit: 15` and a 1 request/minute history gate.
That default follows Slack's strictest documented history policy for newly
created or installed commercially distributed non-Marketplace apps. Marketplace
apps and internal customer-built apps may have different provider limits, but
Locality keeps the default conservative so read-only sync stays provider-safe.

## Write policy

Slack mounts are read-only. Locality rejects edits, creates, renames, moves,
deletes, push writes, undo writes, and autosave writes under Slack mounts.

V1 does not post messages, expand thread bodies, subscribe to Slack events, or
store arbitrary Slack search results.

## Useful commands

```bash
loc connect slack
loc mount slack ~/Locality/slack-main
loc status ~/Locality/slack-main
loc diff ~/Locality/slack-main
loc pull ~/Locality/slack-main
```
