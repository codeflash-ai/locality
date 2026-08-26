# Anonymous Product Telemetry

Locality has a small opt-in telemetry path for product analytics and early bug
discovery. It is intentionally separate from local diagnostics: diagnostic logs
may contain useful local context and are never uploaded by this system.

## Contract

The desktop writes versioned events to `~/.loc/telemetry/` before attempting
delivery. A successful HTTP 2xx response removes the acknowledged batch;
timeouts and non-2xx responses leave it for retry. Batches contain at most 50
events, the local queue retains at most 1,000 events, and PostHog's `$insert_id`
is the client event ID so retries can be deduplicated.

Each envelope contains only:

- a random installation ID and per-process session ID;
- event ID and occurrence time;
- app, version, build, OS, and architecture;
- a machine-readable event name;
- allowlisted low-cardinality properties: `code`, `connector`, `kind`,
  `outcome`, `severity`, `source_file`, and `source_line`.

The type system does not expose an arbitrary properties map. Both the client and
ingestion endpoint reject human text in these fields. Never add file contents,
paths, URLs, titles, queries, account labels, emails, provider object IDs,
tokens, or raw error messages to the contract.

Current desktop events are:

| Event | Meaning |
| --- | --- |
| `diagnostic.recorded` | Existing structured desktop event code and severity; the local message is omitted. |
| `activity.completed` | A durable desktop activity completed, classified by its existing kind. |
| `app.panic` | Rust panic location (source filename and line), without the panic payload. |
| `telemetry.preference_changed` | The user enabled telemetry. |

`app.start` is represented as `diagnostic.recorded` with code `app.start`, so
there is only one path from structured desktop logging to telemetry.

## Consent and lifecycle

Telemetry defaults off. The user can enable **Share anonymous usage and error
reports** in Desktop Settings > General. Disabling it immediately stops capture
and removes queued events. The anonymous installation ID remains local and is
removed with the rest of Locality state during reset or uninstall.

Local diagnostics remain local regardless of this setting. Exporting a support
bundle, if implemented, is a separate explicit user action.

## Delivery and deployment

Set `LOCALITY_TELEMETRY_ENDPOINT` while building the desktop to the first-party
endpoint, for example:

```text
https://oauth.locality.example/v1/telemetry/batch
```

A runtime value with the same name overrides the compiled value for local
development. With no endpoint, capture is inert even when the saved preference
is enabled.

The Cloudflare worker validates the exact schema and forwards accepted batches
to PostHog. Configure:

- `LOCALITY_POSTHOG_PROJECT_KEY` as a Worker secret;
- `LOCALITY_POSTHOG_HOST` when not using `https://us.i.posthog.com`.

The desktop never receives the PostHog project key and can be moved to another
analytics store without changing the client wire contract.

## Adding an event

Prefer a completed outcome or stable failure code that answers a named product
question. Add any new property explicitly to Rust and Worker allowlists, add an
exact contract test, and document it here. Do not emit per-file reads, content
volume, user text, or high-cardinality identifiers. Telemetry failure must never
fail or slow a sync, push, pull, mount, OAuth, or desktop action.
