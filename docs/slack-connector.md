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
`public_channel` join public channels before reading history by default. This
mutates Slack membership for the connected app, and the manifest describes it
separately as `membership_operations: ["join_public_channels"]`. It is not a
content push operation and does not grant message or file write support.
Private channels still require an explicit Slack invite.

The default Slack connector settings are:

```json
{"slack":{"history_limit":15,"types":["public_channel","private_channel","im","mpim"],"auto_join_public_channels":true}}
```

Set `auto_join_public_channels` to `false` in mount settings to avoid the
membership mutation. Unjoined public channels are then omitted rather than
joined or projected; public channels where the app is already a member remain
readable.

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

The hosted provider scopes its process-local gates and cooldowns by the
non-secret Slack app ID, team ID, and exact Web API method. New durable hosts
use `HostedSlackProviderCoordinationScopeV2` for the same exact identity.
`HostedSlackProviderCoordinationScopeV1` and the existing
`coordination_scope(operation)` API retain their original team-and-operation
wire contract for compatibility.

During a durable cooldown migration, hosts must continue reading and honoring
unexpired V1 records while writing new cooldowns under V2 keys. A V1 key cannot
be safely rewritten as V2 because it contains no app ID; V1 records should age
out under their original broader scope instead of being silently dropped or
assigned to an app.

Freshness checks use the bounded conversation history and user metadata payload.
Thread reply bodies are expanded when `recent.md` hydrates, so reply-only edits
become visible on the next hydration or explicit pull without making background
freshness block on `conversations.replies`.

## Write policy

Slack mounts are read-only. Locality rejects edits, creates, renames, moves,
deletes, push writes, undo writes, and autosave writes under Slack mounts.

V1 does not post messages, subscribe to Slack events, or store arbitrary Slack
search results.

## Live E2E

`tests/live_slack_vfs_read.sh` exercises the live Slack API, CLI
mount/pull/status, daemon, and Linux FUSE projection by reading a known
conversation's `recent.md` and verifying Slack remains read-only through the
mounted filesystem and product push validation. The live script refuses
`public_channel` mounts because Slack public-channel reads can auto-join
channels; use a private channel, DM, or group DM where the app is already
present.

To reuse a stored `connection:slack-live` credential in isolated test state:

```bash
secret_ref='connection:slack-live'
secret_hex="$(printf '%s' "$secret_ref" | od -An -tx1 -v | tr -d ' \n')"
export LOCALITY_SLACK_LIVE_CREDENTIAL_JSON="$(cat "$HOME/.loc/credentials/$secret_hex")"
export LOCALITY_SLACK_LIVE_CONVERSATION_ID='G0123456789'
```

Use the full stored credential JSON. The live harness requires
`access_token`, `oauth_broker_url`, `refresh_token_handle`, and numeric
`expires_at` so it can exercise broker refresh when the token expires.

The GitHub live job always forces that refresh assertion and persists the
replacement `LOCALITY_SLACK_LIVE_CREDENTIAL_JSON` environment secret with
`LOCALITY_SECRET_ROTATOR_TOKEN`. Slack refresh tokens are single-use, so a live
state-root credential-store lock coalesces concurrent refresh attempts across
the daemon, CLI, and FUSE processes. After acquiring that lock, Locality rereads
persisted credential state, bypassing the macOS keychain process cache, so
waiting processes reuse the newly rotated credential. The job must not consume
one without saving its replacement for the next serialized run. The live
harness performs its forced refresh before it starts daemon and FUSE consumers,
then validates and exports the replacement credential before continuing with
the mounted-filesystem scenario.

Set `LOCALITY_SLACK_LIVE_TYPES` when the target conversation is not covered by
the default `private_channel,im,mpim` type set. Do not set `public_channel` for
this live test.

```bash
LOCALITY_LIVE_SLACK_VFS=1 tests/live_slack_vfs_read.sh
```

## Useful commands

```bash
loc connect slack
loc mount slack ~/Locality/slack-main
loc status ~/Locality/slack-main
loc diff ~/Locality/slack-main
loc pull ~/Locality/slack-main
```
