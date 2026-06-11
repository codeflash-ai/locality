# CLI Surface

The `afs` command is the single supported control surface for users and coding agents.

## Commands

- `afs connect notion [--name <id>] [--token-stdin] [--json]`
- `afs connections [--json]`
- `afs connection show <id> [--json]`
- `afs disconnect <id> [--json]`
- `afs mount notion <path> --root-page <page-id> [--connection <id>] [--mount-id <id>] [--read-only] [--json]`
- `afs daemon status [--json]`
- `afs info [path] [--json]`
- `afs status [path] [--json]`
- `afs pull <path> [--json]`
- `afs push [path] [-y|--yes] [--confirm] [--json]`
- `afs daemon start|stop|status|restart [--session|--launchd] [--afsd-bin <path>] [--state-dir <path>] [--tcp-addr <host:port|off>] [--include-env <KEY>] [--json]`
- `afs diff [path] [--json]`
- `afs restore <path> [--force] [--json]`
- `afs undo [push-id] [--json]`
- `afs log [path] [--json]`
- `afs resolve --ours|--theirs|--edited <path>`
- `afs config set <key=value>`
- `afs file-provider register|unregister|list|reset [target] [--json]`

## Exit-code contract

Initial numeric assignments:

- `0`: success;
- `1`: internal, I/O, store, connector, auth, or rate-limit failure;
- `2`: usage error;
- `3`: validation error.
- `4`: confirmation, guardrail, or read-only policy required;
- `5`: command reached an intentionally unimplemented or unsupported connector boundary.

Remaining categories to assign before `afs push` applies remote mutations:

- conflict;
- remote concurrency failure.

## Provider Connections

`afs connect notion [--name <id>] [--token-stdin]` creates a local provider connection. It probes Notion with `GET /v1/users/me`, stores only metadata in SQLite, and writes the bearer token to the credential store. JSON output never includes the token or `secret_ref`.

The default connection ID is `notion-default` when no Notion connection exists. If a Notion connection already exists, pass `--name <id>` to avoid overwriting by accident.

`afs connections` and `afs connection show <id>` list metadata only. `afs disconnect <id>` deletes the credential and marks the connection `revoked`; mounts remain registered and will report `connection_revoked` on the next pull/push until reconnected or remounted.

Auth error JSON uses stable codes:

- `missing_connection`: no usable connection and no `NOTION_TOKEN` fallback;
- `auth_required`: connection exists but its credential is missing;
- `connection_revoked`: mount points at a revoked connection;
- `credential_store_unavailable`: keychain or file credential store failed;
- `connection_probe_failed`: Notion rejected the token during `connect`.

Auth failures exit `1` and include `suggested_command` when there is an obvious recovery command.

## Initial `afs mount` and `afs pull`

`afs mount notion <path> --root-page <page-id> [--connection <id>]` creates the local root directory, writes concise source-specific mount guidance to `AGENTS.md`, creates a `CLAUDE.md` alias for agents that read that filename, and stores a mount record in SQLite. Existing guidance files are preserved. With one active Notion connection, mount auto-assigns it. With multiple active Notion connections, pass `--connection <id>`. Existing mounts without `connection_id` continue to work through the legacy `NOTION_TOKEN` fallback.

`afs mount notion <path> --root-page <page-id> --projection macos-file-provider` records a macOS File Provider mount. Scheduled pull for this projection updates SQLite metadata and queues hydration, but does not write placeholder Markdown bodies. The File Provider extension lists dataless files from the daemon and materializes a file on open.

`afs file-provider register <mount-id-or-path>` registers a macOS File Provider domain for a mount whose projection is `macos_file_provider`. `unregister` removes that domain, `list` shows domains known to File Provider, and `reset` removes all domains for the installed AgentFS provider. The command shells out to the signed app-bundle helper at `AgentFS.app/Contents/MacOS/agentfs-file-providerctl`; `AFS_FILE_PROVIDERCTL` can override the helper path for development.

`afs pull <mount-root>` enumerates the configured Notion root page. For plain-file mounts it writes stub Markdown files for projected pages, creates directories for projected databases, writes database `_schema.yaml` files, enumerates database row stubs with property frontmatter, hydrates the root page, downloads image media under `media/`, and persists the root page shadow snapshot. For macOS File Provider mounts it leaves unhydrated entries online-only and only writes content when hydration is requested. `afs pull <page-file>` hydrates one known entity and downloads its image media. Pull refuses to overwrite a hydrated file if its body no longer matches the stored shadow, returning a dirty skip instead.

The JSON report includes `via`, `enumerated`, `stubbed`, `hydrated`, and `skipped_dirty` counts. `via` is `daemon` when the Unix socket handled the job and `cli` when the command executed directly.

If the daemon socket is unavailable and `AFS_DAEMON_DISABLE` is not set, pull/push print a stderr hint and continue directly:

```text
afsd not running; executing pull directly (start afsd for background hydration)
```

## Daemon Process Management

`afs daemon start` starts `afsd` as a background daemon. On macOS, the default
manager is a per-user LaunchAgent at
`~/Library/LaunchAgents/ai.codeflash.afs.afsd.plist`, with stdout/stderr under
`~/.afs/logs/`. The LaunchAgent uses `RunAtLoad` and `KeepAlive`, so the daemon
starts at login and launchd restarts it if it exits. On non-macOS systems, or
when `--session` is passed, the CLI starts a detached child process and writes
`~/.afs/afsd.pid`; session mode inherits the current shell environment but does
not survive logout.

Useful forms:

```sh
afs daemon start
afs daemon start --session
afs daemon status
afs daemon stop
afs daemon restart
```

`--state-dir <path>` starts or queries a daemon for an isolated state root.
`--tcp-addr <host:port|off>` persists the TCP listener setting for that managed
daemon. `--afsd-bin <path>` points the manager at a specific daemon binary. For
development-only environment variables that launchd would not otherwise know
about, `--include-env <KEY>` copies the current value into the LaunchAgent plist;
do not use it for long-lived secrets once keychain-backed `afs connect` is
available.

## Initial `afs info --json` Shape

`afs info [path]` explains the local source-of-record context for one path using only the SQLite state store. It defaults to the current working directory, resolves the containing mount, identifies the exact or nearest projected entity, reports immediate child counts, and includes pending/failed journal counts for that local context. It does not read or hydrate file bodies and does not call remote connectors.

The JSON report includes:

- `mount`: mount ID, connector, root, remote root ID, and read-only state;
- `subject`: role, source type, local path, existence, backing entity metadata, and database schema path when applicable;
- `children`: immediate child counts by entity type plus subtree entity count;
- `journals`: pending and failed journal counts in the context.

Human output is a compact path summary for people and agents working in nested directories.

## Initial `afs status --json` Shape

`afs status [path]` inspects local mount state only. It resolves the target path through the stored mount/entity mapping, compares hydrated page bodies against their stored shadow snapshots, reports stubs and missing/conflicted projections, and includes pending or failed push journals touching each entity. It does not call remote connectors.

The production state directory defaults to `~/.afs`; `AFS_STATE_DIR` is a developer/test override for isolated runs. When no path is supplied, `afs status` first checks the current working directory: inside a mount it scopes to that subtree, and outside all mounts it reports every registered mount in the active state directory.

The JSON report includes:

- `clean`: false when any entry is stubbed, dirty, conflicted, missing, errored, or has pending/failed journals;
- `summary`: counts by state plus pending/failed journal counts;
- `mounts[].entries[]`: path, entity ID, kind, title, hydration state, status state, issues, and journal counts.

Human output lists only non-clean entries and ends with a compact summary, or prints a clean line when every tracked entry is clean.

Non-clean human entries are multi-line so failed journals expose their recovery context:

```text
notion-main  initial-idea ~37b3ac.md
  state: dirty  hydration: dirty
  issue: entity_dirty - entity is marked dirty
  issue: failed_journal - 2 push journal(s) failed
  last_failure: unsupported feature: moving Notion blocks
```

## Initial `afs diff --json` Shape

The first diff implementation resolves a path through the store, reads the canonical Markdown file, loads its shadow snapshot, and returns the core push-pipeline decision without applying anything. If the path is a new Markdown file directly inside a projected database directory, it plans a `create_entity` operation for a new database row instead of requiring an existing shadow. The JSON report includes:

- `validation`: machine-readable issues with file, line, message, and suggested fix;
- `plan.summary`: block/entity/property counts;
- `plan.operations`: connector-neutral planned mutations;
- `plan.degradations`: explicit fidelity warnings from the diff planner;
- `guardrail`: `proceed` or `confirm_required`;
- `action`: the next push action, such as `noop`, `confirm_plan`, `confirm_dangerous_plan`, or `fix_validation`.

The production command path uses the SQLite store. A real diff requires persisted mount, entity, and shadow rows for the target path.

## Initial `afs push --json` Shape

The push implementation runs the same path resolution, parsing, validation, diffing, and guardrail evaluation as `afs diff`. When the plan is approved, it enters the journaled connector-apply executor. It supports `-y`/`--yes` for safe plans and `--confirm` for dangerous plans.

The JSON report has the same validation, plan, degradation, guardrail, and stage fields as `afs diff`. Its `action` is one of:

- `fix_validation`;
- `noop`;
- `confirm_plan`;
- `confirm_dangerous_plan`;
- `read_only_blocked`;
- `unsupported_operations`;
- `reconciled`;
- `apply_not_implemented`;
- `apply_failed`.

Reports also include `via`, `push_id`, `journal_status`, changed/reconciled remote IDs, and `apply_effect_count` when execution starts. The Notion connector now applies the supported block and page-property write subset, block moves, and new database-row creation through the live API. Connector capability preflight runs before journaling, so unsupported operations return `unsupported_operations` without appending a journal.

Unsupported-operation JSON shape:

```json
{
  "action": "unsupported_operations",
  "unsupported": ["archive_entity"],
  "message": "Notion connector cannot apply: archive_entity",
  "suggested_fix": "Reorder edits to append-only, or wait for connector support"
}
```

`unsupported_operations`, `apply_not_implemented`, and other unsupported connector boundaries exit `5`.

## `afs restore`

`afs restore <path> [--force] [--json]` is a local recovery command. It resolves the path to a mounted entity, loads the last stored shadow, rewrites the file atomically from canonical frontmatter plus the shadow body, refreshes the entity content hash, and marks hydration back to `hydrated`. It does not call Notion and does not delete failed journals, so `afs log` remains an audit trail.

`afs restore` refuses conflicted entities unless `--force` is supplied.

Typical recovery:

```bash
afs status ~/afs/notion
afs restore ~/afs/notion/initial-idea\ ~37b3ac.md
afs status ~/afs/notion
```

## `afs daemon status`

`afs daemon status [--json]` checks the configured Unix socket and sends `DaemonRequest::Ping` when a socket is present.

Human output:

```text
daemon running  socket=/Users/alice/.afs/afsd.sock  ping=ok
```

or:

```text
daemon stopped  socket=/Users/alice/.afs/afsd.sock
  hint: run `afsd` in another terminal
```

## Initial `afs log --json` Shape

`afs log [path]` reads the durable push journal from the SQLite state store. Without a path it lists all journal entries; with a path it resolves the path through the mount/entity mapping and lists entries that touched that entity.

Each JSON entry includes:

- `push_id`;
- `mount_id`;
- `remote_ids`;
- `status`: `prepared`, `applying`, `applied`, `reconciled`, `reverted`, or `failed`;
- `failure`: the failed status message when present;
- `preimage_count`;
- `apply_effect_count`;
- `plan_summary`;
- `operation_count`.

Human output is a compact git-log-style list headed by `push <push-id>`.

## Initial `afs undo --json` Shape

`afs undo <push-id>` reads one journal entry and returns an undo decision. The current safe behavior is:

- `prepared` entries become `reverted` because no remote mutation has started;
- `reverted` entries return `already_reverted`;
- `applied` and `reconciled` entries derive an `undo_plan` from journaled preimages and apply effects;
- complete plans are handed to the connector reverse-apply hook, then marked `reverted` on success;
- Notion currently returns `reverse_apply_not_implemented` with a `NotImplemented` message until its reverse API implementation exists;
- `applying` and `failed` entries return `undo_unsafe_journal_status` because partial remote effects may still be in flight or unknown.

Undo plans are `complete`, `partial`, or `blocked`. Complete plans currently include reverse operations for block updates, block moves, archived blocks, appended blocks with journaled created IDs, and created entities with journaled created IDs. Property updates and archived entities remain explicitly unsupported until richer property/entity preimages are journaled.

## Manual / Live Verification

Ignored live test:

```bash
NOTION_TOKEN=... AFS_NOTION_PAGE_ID=... \
  cargo test -p afs-cli --test e2e_push_workflow live_mid_page_insert_push_reconciles -- --ignored
```

The test mounts a temporary Notion projection, pulls the root page, inserts a paragraph, pushes with confirmation, and expects the push to reconcile.
