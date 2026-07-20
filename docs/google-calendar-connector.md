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
