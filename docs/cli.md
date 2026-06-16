# CLI Surface

The `afs` command is the single supported control surface for users and coding agents.

## Commands

- `afs connect notion [--name <id>] [--token-stdin|--no-browser|--direct-oauth] [--broker-url <url>] [--redirect-uri <uri>] [--json]`
- `afs connections [--json]`
- `afs profiles [--json]`
- `afs connection show <id> [--json]`
- `afs disconnect <id> [--json]`
- `afs mount notion <path> --root-page <page-id> [--connection <id>] [--mount-id <id>] [--projection plain-files|macos-file-provider|linux-fuse] [--read-only] [--json]`
- `afs daemon status [--json]`
- `afs info [path] [--json]`
- `afs status [path] [--json]`
- `afs inspect <path> [--json]`
- `afs pull <path> [--json]`
- `afs push [path] [-y|--yes] [--confirm] [--json]`
- `afs daemon start|stop|status|reload|restart [--session|--launchd] [--afsd-bin <path>] [--state-dir <path>] [--tcp-addr <host:port|off>] [--include-env <KEY>] [--json]`
- `afs diff [path] [--json]`
- `afs restore <path> [--force] [--json]`
- `afs undo [push-id] [--json]`
- `afs log [path] [--json]`
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

`afs connect notion [--name <id>]` creates a local provider connection. OAuth is preferred. By default the command uses the AFS OAuth broker because Notion's REST OAuth token endpoint requires a confidential client secret. The CLI asks the broker for a Notion authorization URL, opens the browser, listens for the localhost callback, sends only the returned authorization code plus signed broker session back to the broker, then stores the returned access token and refresh handle in the credential store. SQLite stores only connection metadata and a `secret_ref`.

The default broker is `https://afs-oauth-broker.saurabh-b07.workers.dev`; override it with `--broker-url <url>`, `AFS_NOTION_OAUTH_BROKER_URL`, or `AFS_AUTH_BROKER_URL`. The default callback is `http://localhost:8757/oauth/notion/callback`; override it with `--redirect-uri <uri>` or `AFS_NOTION_OAUTH_REDIRECT_URI`. The redirect URI must be registered on the Notion public integration.

`--direct-oauth` keeps the developer BYO OAuth path. In that mode the command reads `AFS_NOTION_OAUTH_CLIENT_ID` and `AFS_NOTION_OAUTH_CLIENT_SECRET` (or `NOTION_OAUTH_CLIENT_ID` / `NOTION_OAUTH_CLIENT_SECRET`) and exchanges directly with Notion. Direct OAuth stores the user-supplied client secret in the credential store so refresh can work, and should not be the default product path.

`--no-browser` prints the authorization URL but does not try to open it. `--token-stdin` is the explicit personal-access-token fallback for local development and CI:

```bash
echo "$NOTION_TOKEN" | afs connect notion --token-stdin --name work
```

JSON output never includes OAuth tokens, refresh tokens, client secrets, PATs, or `secret_ref`.

Connections now point at connector profiles. A profile is AgentFS's local auth-config record: connector, auth kind, scopes, enabled action classes, connector version, status, and capabilities. OAuth connections use `notion-oauth-default`; explicit PAT connections use `notion-token-default`.

The default connection ID is `notion-default` when no Notion connection exists. If a Notion connection already exists, pass `--name <id>` to avoid overwriting by accident.

`afs connections` and `afs connection show <id>` list connected-account metadata only, including the profile ID but never credentials. `afs profiles` lists connector auth profiles and contains no account secrets. `afs disconnect <id>` deletes the credential and marks the connection `revoked`; mounts remain registered and will report `connection_revoked` on the next pull/push until reconnected or remounted.

Auth error JSON uses stable codes:

- `missing_connection`: no usable connection and no `NOTION_TOKEN` fallback;
- `auth_required`: connection exists but its credential is missing;
- `connection_revoked`: mount points at a revoked connection;
- `auth_profile_unavailable`: connection points at a missing, inactive, or mismatched connector profile;
- `credential_store_unavailable`: keychain or file credential store failed;
- `missing_oauth_config`: direct OAuth was requested but the Notion OAuth client ID or client secret was not configured;
- `oauth_broker_start_failed`: the configured OAuth broker could not create a Notion authorization session;
- `oauth_exchange_failed`: Notion rejected the OAuth authorization code exchange;
- `connection_probe_failed`: Notion rejected the token during `connect`.

Auth failures exit `1` and include `suggested_command` when there is an obvious recovery command.

## Initial `afs mount` and `afs pull`

`afs mount notion <path> --root-page <page-id> [--connection <id>]` creates the local root directory, writes concise source-specific mount guidance to `AGENTS.md`, creates a `CLAUDE.md` alias for agents that read that filename, and stores a mount record in SQLite. Existing guidance files are preserved. With one active Notion connection, mount auto-assigns it. With multiple active Notion connections, pass `--connection <id>`. Existing mounts without `connection_id` continue to work through the legacy `NOTION_TOKEN` fallback.

Projection choices are platform-specific. Linux binaries accept `plain-files` and
`linux-fuse`; macOS binaries accept `plain-files` and `macos-file-provider`;
Windows currently accepts `plain-files` only.

`afs mount notion <path> --root-page <page-id> --projection macos-file-provider` records a macOS File Provider mount. `--projection linux-fuse` records the equivalent Linux virtual projection for the FUSE helper. Scheduled pull for virtual projections updates SQLite metadata and queues hydration, but does not write placeholder Markdown bodies. The File Provider extension lists dataless files from the daemon and materializes a file on open.

Linux should expose the same online-only behavior through a FUSE projection
helper rather than through inotify-triggered placeholder files. The daemon API for
that path is platform-neutral `virtual_fs`; macOS File Provider commands are
compatibility aliases over it.

`afs file-provider register <mount-id-or-path>` validates the mount against the
current platform's virtual projection: `macos_file_provider` on macOS and
`linux_fuse` on Linux. The macOS path shells out to the signed app-bundle helper
at `AgentFS.app/Contents/MacOS/agentfs-file-providerctl`; `AFS_FILE_PROVIDERCTL`
can override the helper path for development. The Linux path writes and starts a
per-mount systemd user service for `afs-fuse`; `AFS_FUSE_BIN` can override the
helper binary path for development. `afs file-provider unregister <mount>` stops
and removes that Linux service. `list` and `reset` still target the macOS File
Provider helper.

`afs pull <mount-root>` enumerates the configured Notion root page. For plain-file mounts it writes stub Markdown files for projected pages, creates directories for projected databases, writes database `_schema.yaml` files, enumerates database row stubs with property frontmatter, hydrates the root page, downloads image media under `media/`, and persists the root page Synced Tree shadow snapshot. For virtual filesystem mounts it leaves unhydrated entries online-only and only writes content when hydration is requested. `afs pull <page-file>` hydrates one known entity and downloads its image media. Pull refuses to overwrite a hydrated file if its body no longer matches the Synced Tree shadow, returning a dirty skip instead.

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
afs daemon reload
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

`afs status [path]` inspects Local Tree state and the latest Remote Tree metadata
the daemon has already observed. It resolves the target path through the stored
mount/entity mapping, compares hydrated page bodies against their Synced Tree
shadow snapshots, reports stubs, conflicted files with unresolved inline markers,
dirty files, missing projections, and pending or failed push journals touching
each entity. It does not call remote connectors itself.

The production state directory defaults to `~/.afs`; `AFS_STATE_DIR` is a developer/test override for isolated runs. When no path is supplied, `afs status` first checks the current working directory: inside a mount it scopes to that subtree, and outside all mounts it reports every registered mount in the active state directory.

The JSON report includes:

- `clean`: false when any entry is stubbed, dirty, conflicted, missing, errored, or has pending/failed journals;
- `summary`: counts by local state, pending/failed journal counts, and sync safety states;
- `mounts[].entries[]`: path, entity ID, kind, title, hydration state, Local Tree status state, sync safety state, latest Remote Tree observation metadata, issues, and journal counts.

`state` is the Local Tree file/projection state: `clean`, `stub`, `dirty`,
`conflicted`, `missing`, or `error`. `sync_state` is the higher-level safety
state for humans and agents:

- `all_synced`: no known Local Tree pending change or Remote Tree drift;
- `checking_freshness`: AFS has Local Tree activity and is checking Remote Tree metadata;
- `remote_update_available`: Remote Tree metadata moved while the Local Tree file is clean;
- `pending_local_changes`: Local Tree edits are waiting for review/push;
- `review_needed`: both Local Tree and Remote Tree changed, or the projection needs manual attention;
- `conflicted`: unresolved conflict markers or conflicted entity state.

The summary stores the conflicted sync-state count as `sync_conflicted` because
`conflicted` already names the local file/projection state count.

Human output lists entries that need attention and ends with a compact summary,
or prints a clean line when every tracked entry is safe. If AFS is only checking
freshness in the background, the clean line includes the number of entries being
checked instead of listing each file.

Non-clean human entries are multi-line so failed journals expose their recovery context:

```text
notion-main  initial-idea ~37b3ac.md
  state: dirty  sync: pending_local_changes  hydration: dirty
  issue: entity_dirty - entity is marked dirty
  issue: failed_journal - 2 push journal(s) failed
  last_failure: unsupported feature: moving Notion blocks
```

## Initial `afs inspect --json` Shape

`afs inspect <path>` is an explicit Remote Tree change explanation barrier for one
hydrated page. Unlike `afs status`, it is allowed to call the connector. It
compares:

- the Synced Tree shadow;
- the current Local Tree Markdown file or virtual projection content cache;
- a freshly rendered Remote Tree document.

The command does not mutate local files, shadows, freshness metadata, or remote
content. It is intended for review flows where status already says a remote
update may exist, or where a human/agent wants an authoritative explanation
before deciding whether to push, pull, or manually merge.

JSON output includes:

- `state`: `all_synced`, `local_changed_only`, `remote_changed_only`,
  `both_changed`, or `needs_review`;
- `action`: `none`, `push_local_changes`, `safe_to_fast_forward`, or
  `review_before_push`;
- `local` and `remote`: whether each tree changed relative to the Synced Tree shadow, plus
  the connector-neutral plan when planning succeeds;
- `issues`: parse, path, or planning problems that require manual review.

Human output is a compact summary:

```text
inspect /Users/alice/Library/CloudStorage/AgentFS-Notion/Roadmap.md
  mount: notion-main  entity: page-1
  title: Roadmap
  Synced Tree version: 2026-06-10T00:00:00Z
  Remote Tree version: 2026-06-11T00:00:00Z
  state: remote_changed_only  action: safe_to_fast_forward
  local: unchanged (0 operations)
  remote: changed (1 operation)
```

## Initial `afs diff --json` Shape

The first diff implementation resolves a path through the store, reads the canonical Markdown file, loads its Synced Tree shadow snapshot, and returns the core push-pipeline decision without applying anything. If the file contains unresolved inline conflict markers, validation returns `unresolved_conflict_markers` before planning. If the path is a new Markdown file directly inside a projected database directory, it plans a `create_entity` operation for a new database row instead of requiring an existing shadow. The JSON report includes:

- `validation`: machine-readable issues with file, line, message, and suggested fix;
- `plan.summary`: block/entity/property counts;
- `plan.operations`: connector-neutral planned mutations;
- `plan.degradations`: explicit fidelity warnings from the diff planner;
- `guardrail`: `proceed` or `confirm_required`;
- `action`: the next push action, such as `noop`, `confirm_plan`, `confirm_dangerous_plan`, or `fix_validation`.

The production command path uses the SQLite store. A real diff requires persisted mount, entity, and shadow rows for the target path.

## Initial `afs push --json` Shape

The push implementation runs the same path resolution, parsing, validation, diffing, and guardrail evaluation as `afs diff`. It refuses `unresolved_conflict_markers`; edit the file to the intended final content and remove every marker line before pushing. When the plan is approved, it enters the journaled connector-apply executor. It supports `-y`/`--yes` for safe plans and `--confirm` for dangerous plans.

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

Reports also include `via`, `push_id`, `journal_status`, changed/reconciled remote IDs, and `apply_effect_count` when execution starts. The Notion connector now applies the supported block and page-property write subset, block moves, and new database-row creation through the live API. Connector capability preflight runs before journaling, so unsupported operations return `unsupported_operations` without appending a journal. Once a journaled push starts, the daemon performs connector metadata checks and verifies the current Remote Tree render still matches the Synced Tree shadow before applying Local Tree edits.

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

`afs restore <path> [--force] [--json]` is a local recovery command. It resolves the path to a mounted entity, loads the Synced Tree shadow, rewrites the file atomically from canonical frontmatter plus the shadow body, refreshes the entity content hash, and marks hydration back to `hydrated`. It removes inline conflict markers by overwriting the file from shadow. It does not call Notion and does not delete failed journals, so `afs log` remains an audit trail.

`afs restore` refuses legacy conflicted entities unless `--force` is supplied.

Typical recovery:

```bash
afs status ~/afs/notion
afs restore ~/afs/notion/initial-idea\ ~37b3ac.md
afs status ~/afs/notion
```

## `afs daemon status`

`afs daemon status [--json]` checks the configured Unix socket and, when the daemon is running, requests a daemon status snapshot. JSON output includes process-manager state, runtime queue counts, scheduler mode, watched mount count, and watched roots.
Runtime queue counts include mutating requests, hydration work, scheduled pulls,
and freshness work. Freshness metrics report pending/ready/deferred jobs plus
ready and total budget units, which helps diagnose sync pressure without
requiring a full workspace scan.

`afs daemon reload [--json]` tells a running daemon to reconcile its watched mount roots with the current SQLite mount table. `afs mount` sends the same IPC request after saving a new mount, so a persistent daemon starts watching newly mounted directories without a restart.

Human output:

```text
daemon running
  state: running
  manager: launchd
  watched mounts: 2
  jobs: active=false, pending=0, hydration=0
  scheduler: polling
```

or:

```text
daemon stopped
  state: stopped
  socket: /Users/alice/.afs/afsd.sock
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
NOTION_TOKEN=... AFS_NOTION_LIVE_PARENT_PAGE=... \
  cargo test -p afs-cli --test e2e_push_workflow live_scratch_page_mount_edit_push_verifies_notion -- --ignored --exact
```

The test creates a scratch page under the configured live parent, mounts a temporary Notion projection, pulls the root page, edits the local Markdown file, verifies pending status, pushes with confirmation, verifies the edit through the Notion API, and archives the scratch page.
