# Google Calendar Connector Summary

This document summarizes the first Google Calendar connector implementation.

## Connector Scope

Google Calendar is registered as a first-party Locality source connector named
`google-calendar`. It uses the normal Locality connection, mount, pull, hydrate,
diff, push, status, and live projection paths.

The V1 connector is scoped to the account's primary calendar only.

The connector projects a fixed calendar shape:

```text
~/Locality/
  google-calendar-main/
    events/
    draft/
```

`events/` is read-only. `draft/` is the local write surface for new primary
calendar events.

## OAuth

The Google Calendar OAuth flow uses the Locality OAuth broker endpoints:

- `/v1/oauth/google-calendar/start`
- `/v1/oauth/google-calendar/exchange`
- `/v1/oauth/google-calendar/refresh`

The default callback is:

```text
http://localhost:8757/oauth/google-calendar/callback
```

The broker allowlist also supports:

```text
http://127.0.0.1:8757/oauth/google-calendar/callback
```

Google Calendar, Gmail, and Google Docs share the same broker-configured Google
OAuth client: `LOCALITY_GOOGLE_CLIENT_ID` and
`LOCALITY_GOOGLE_CLIENT_SECRET`. Register each connector's localhost callback
on that Google OAuth client.

The broker requests these scopes:

- `openid`
- `email`
- `profile`
- `https://www.googleapis.com/auth/calendar.events.owned`

`calendar.events.owned` is used because V1 reads existing events from the
account's primary calendar and creates reviewed draft events on that same
primary calendar. The connector does not operate on non-owned calendars or
request full all-calendar event access.

## Google OAuth Verification

Keep the Google Cloud Console verification scope list aligned with
`connectors/oauth-verification/google-calendar.json`.

Submit only this Google API scope:

- `https://www.googleapis.com/auth/calendar.events.owned`

Do not submit full calendar, all-calendar events, readonly-only, app-created
only, freebusy, or settings scopes for this connector.

The verification demo should show the OAuth consent screen with all requested
permissions readable, then demonstrate the code-backed user workflows: mounting
Google Calendar, enumerating existing primary-calendar events, opening a pulled
event with its details, creating a draft event under `draft/`, pushing that
reviewed draft, and showing the created event in the user's primary Google
Calendar account. Update and delete workflows are not exposed in Google
Calendar V1, so the demo should not imply they are available.

## Projection And Pull

By default, Pull enumerates a rolling date window from 30 days back through 180
days forward. Recurring events are expanded through the Google Calendar Events
API with `singleEvents=true`.

Google Calendar mounts can be registered with an explicit date window:

```bash
loc mount google-calendar ~/Locality/google-calendar-main \
  --after 2026-07-01 \
  --before 2026-07-31
```

Event files are projected under `events/`:

```text
google-calendar-main/
  events/
    2026-07-20-design-review-event-1.md
  draft/
```

Rendered event frontmatter includes Locality identity, readable event fields
such as `summary`, `start`, `end`, and `location`, and the full Google Calendar
event resource under `google_calendar.event`. The Markdown body is the event
description.

## Hosted Portable Ingestion

The public connector supports hosted portable ingestion for the primary
calendar only. The portable scope root is exactly
`google-calendar:primary`; other calendar roots are rejected.

Portable bootstrap requires a persisted explicit `--after`/`--before` date
window. It uses those fixed dates for the full primary-calendar inventory and
returns them in the connector checkpoint. A mount using the local rolling
30-day-back/180-day-forward default is rejected before any Google Calendar API
call, because its scope is not stable enough for hosted inventory.

Portable bootstrap is initial-inventory only and rejects supplied checkpoints.
It expands all pages in the fixed window, skips cancelled events, deduplicates
identical remote IDs across pagination, and rejects conflicting duplicate
identity, version, or path observations. Portable fetch also requires that same
persisted window and rejects cancelled or out-of-window events before emitting
an artifact. Bootstrap emits read-only event source objects with canonical event
remote versions. When `max_changes` truncates the inventory, the result is incomplete with
`google_calendar_bootstrap_max_changes_exceeded`; provider and pagination
errors are returned rather than treated as complete. Portable fetch validates
that the returned event remains a primary-calendar event with the requested
remote ID. Portable render stores the native event JSON as the canonical
artifact and emits one read/search-only Markdown projection.

## Write Policy

`events/` is read-only. File Provider and source write policy should reject
edits and deletes there.

Creating a Markdown file directly under `draft/` is writable:

```text
draft/design-review.md
```

Nested draft files are rejected:

```text
draft/team/design-review.md
```

Draft frontmatter requires `summary` or `title`, plus `start` and `end` objects
in the native Google Calendar API shape.

```markdown
---
summary: Design review
location: Room 12
start:
  dateTime: "2026-07-20T10:00:00-07:00"
  timeZone: America/Los_Angeles
end:
  dateTime: "2026-07-20T10:30:00-07:00"
  timeZone: America/Los_Angeles
attendees:
  - email: ann@example.com
  - email: lee@example.com
google_calendar:
  conference: google_meet
---

Agenda:

- Review launch scope
- Confirm owners
```

`loc push` for a Google Calendar draft creates an event on the primary calendar
with `sendUpdates=all`. Setting `google_calendar.conference: google_meet`
requests a Google Meet link with `conferenceDataVersion=1`.

## Live E2E

`tests/live_google_calendar_vfs_roundtrip.sh` exercises the live Google
Calendar API, CLI mount/pull/diff/push path, daemon, and Linux FUSE projection.
It creates a scratch draft event through the mounted `draft/` directory,
verifies the pushed event appears under `events/`, and deletes the event through
Google Calendar API cleanup.

To reuse a stored `connection:google-calendar-live` credential:

```bash
secret_ref='connection:google-calendar-live'
secret_hex="$(printf '%s' "$secret_ref" | od -An -tx1 -v | tr -d ' \n')"
export LOCALITY_GOOGLE_CALENDAR_LIVE_CREDENTIAL_JSON="$(cat "$HOME/.loc/credentials/$secret_hex")"
```

Use the full stored credential JSON. The live harness requires
`access_token`, `oauth_broker_url`, `refresh_token_handle`, and numeric
`expires_at` so it can exercise broker refresh when the token expires.

Run the gated live test:

```bash
LOCALITY_LIVE_GOOGLE_CALENDAR_VFS=1 tests/live_google_calendar_vfs_roundtrip.sh
```

## Useful Commands

Connect with the local broker:

```bash
loc connect google-calendar --name google-calendar-default
```

Mount Google Calendar:

```bash
loc mount google-calendar ~/Locality/google-calendar-main --projection plain-files
```

Force enumeration:

```bash
loc pull ~/Locality/google-calendar-main
```

Review and create a draft event:

```bash
loc status "$HOME/Locality/google-calendar-main/draft/design-review.md"
loc diff "$HOME/Locality/google-calendar-main/draft/design-review.md"
loc push "$HOME/Locality/google-calendar-main/draft/design-review.md" -y
```

## Current Limitations

- Primary calendar only.
- Existing events are read-only.
- Remote Google Calendar drafts are not projected.
- Calendar attachments are rendered as metadata but not uploaded from Locality
  drafts in V1.
- Incremental sync tokens are not persisted; Pull uses bounded date-window
  enumeration.
