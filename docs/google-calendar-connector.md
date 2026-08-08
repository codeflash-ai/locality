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
- `https://www.googleapis.com/auth/calendar.events`

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

`tests/live_google_calendar_scenario.sh` is the broader local bug-finding
scenario. It seeds scratch timed, all-day, meeting, and recurring events through
the Google Calendar API, pulls them through the Locality mount, creates a new
event from `draft/`, verifies the provider event, patches a seeded event through
the provider API and verifies Locality reads the update back, then confirms
local edits to projected `events/` files are blocked as read-only. Set
`LOCALITY_GOOGLE_CALENDAR_LIVE_ATTENDEE_EMAIL` to add an attendee to the seeded
meeting without sending updates.

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

Run the broader local scenario:

```bash
LOCALITY_LIVE_GOOGLE_CALENDAR_SCENARIO=1 tests/live_google_calendar_scenario.sh
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
