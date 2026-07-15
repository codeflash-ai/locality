# Granola Connector

Locality's Granola connector uses Granola's supported public REST API and is
strictly read-only. It does not inspect Granola's encrypted desktop database,
decrypt its local session, use private application endpoints, or write meeting
notes.

## Setup

Granola API keys are available on Business and Enterprise plans. Create one in
**Granola Settings → Connectors → API keys** with the personal-notes and/or
public-notes scopes appropriate for the mount.

```bash
printf '%s' "$GRANOLA_API_KEY" | loc connect granola --api-key-stdin
loc mount granola ~/Library/CloudStorage/Locality/granola
```

The desktop **Add Source** dialog supports the same flow and creates the
read-only `granola-main` mount automatically.

## Filesystem Contract

Meetings are flat and sort chronologically by their Granola creation time:

```text
granola/
  2026-07-14T173000Z--weekly-product-sync--not_1d3tmYTlCICgjy/
    summary.md
    transcript.md
```

The title is slugged and capped, while the full note ID provides stable
identity. Both files contain Locality identity plus Granola metadata for the
owner, attendees, calendar event, folder ancestry, timestamps, and web URL.

`summary.md` prefers Granola's Markdown summary and falls back to its plain-text
summary. `transcript.md` preserves every returned chunk in order with absolute
timestamps, speaker identity or diarization label, and microphone/speaker
source. Chunks are not combined or rewritten.

Granola can permanently delete transcripts under an individual or workspace
retention policy while retaining the note summary. Locality therefore keeps a
stable `transcript.md` file that explains when no transcript was returned.

## Sync and Limits

The first successful discovery enumerates all accessible notes through cursor
pagination. Locality stores a versioned, mount-scoped discovery checkpoint.
Later discovery requests use Granola's `updated_after` filter with a two-day
overlap so delayed edits and meetings near a date boundary are not missed.
The returned identities are merged into durable entity state, making the
overlap safe. Summary and transcript files still hydrate lazily when opened.

While the daemon's background connector sync is enabled, it schedules a
Granola root discovery every five minutes. The check is independent of the
normal pull scheduler and refreshes only the mount root; it does not recursively
rescan every cached meeting directory. A failed periodic check waits for the
next interval rather than creating a tight durable retry loop.

Granola currently documents a burst capacity of 25 requests and a sustained
rate of 5 requests per second. Locality intentionally defaults to a smoother
5 requests per second, burst 3, and at most 8 Granola requests in flight. Safe
GET requests retry transient transport failures, HTTP 408/429, and selected 5xx
responses up to four times. `Retry-After` is honored when Granola supplies it;
otherwise retry delay grows exponentially from one to sixteen seconds.

These values are connector policy in code rather than user-facing environment
settings. Changes should be reviewed and tested as connector behavior changes.

The public API has no webhooks, so remote changes are discovered through the
periodic incremental check, an explicit pull, or normal read-only freshness
work. Every projected item is marked read-only and all create, edit, rename,
move, delete, push, and auto-save paths are rejected locally.

The public API does not expose manual-note or summary writes. Granola's MCP is
also read-only. Write support must wait for a supported public API rather than
depending on the private `update-document` implementation used by Granola's
desktop client.

## API References

- <https://docs.granola.ai/api-reference/list-notes>
- <https://docs.granola.ai/api-reference/get-note>
- <https://docs.granola.ai/help-center/sharing/integrations/granola-api>
- <https://docs.granola.ai/help-center/consent-security-privacy/transcript-auto-deletion>
