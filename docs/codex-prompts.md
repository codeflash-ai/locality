# Codex Task Prompts

## Changelog

- 2026-06-17: Added template apply drafts, broader macOS File Provider path matching, and desktop status/push affordances for pending Notion changes.
- 2026-06-17: Added the first local template-pack foundation with bundled Founder Proof of Work and Focused Inbox packs plus `afs templates list|validate|new`.
- 2026-06-16: Added search-result safety labels so future MCP/agent readers can distinguish clean hydrated content from metadata-only, stale, dirty, conflicted, or deleted results.
- 2026-06-16: Added a rebuildable SQLite FTS candidate index for local metadata search while preserving the shared CLI/desktop search report contract.
- 2026-06-16: Added hydration-on-locate plumbing: explicit daemon hydration requests and desktop locate prioritization for online-only pages.
- 2026-06-16: Queued the next implementation slices after local metadata search: desktop shared-search adoption, hydration-on-locate, SQLite FTS, knowledge bundles, security labels, MCP, and templates.
- 2026-06-16: Added local metadata search direction with `afs search`, connector filtering, remote-observation safety labels, and regression coverage.
- 2026-06-11: Added end-to-end local Notion OAuth connect flow with localhost callback, OAuth credential bundles in the credential store, PAT fallback, refresh support, and docs.
- 2026-06-11: Started state-of-the-art connector/auth hardening by adding connector profiles/auth-config records, SQLite v9 migration, profile-aware Notion connections, and `afs profiles`.
- 2026-06-11: Added first block-support follow-up: callout write/apply support, Tier 1 append regression coverage, and updated Notion block support docs.
- 2026-06-11: Completed production-hardening sprint phases A-E: Notion block move apply, push preflight, restore/status recovery UX, local provider connections, daemon status/via reporting, and E2E push workflow regression coverage.

## Implementation Queue

Keep this queue ordered by user-visible value and risk. Each slice should remain
small enough to ship with `cargo fmt --all -- --check`, `cargo test --workspace`,
and clippy when the workspace is already clippy-clean.

1. **Desktop shared search backend** — make the desktop locate/typeahead path use
   `afs_cli::search` so CLI, app, and future agent surfaces share one ranking
   and state-label contract.
2. **Hydration on locate** — when search/locate finds an online-only entity,
   enqueue or request high-priority hydration without waiting for a full
   workspace sync.
3. **Search index hardening follow-up** — extend the derived SQLite search
   index beyond entity/remote-observation metadata into breadcrumbs, aliases,
   recent activity, and safe frontmatter fields. It must remain rebuildable and
   must not store secrets.
4. **Markdown body FTS** — add eventually consistent full-text search over
   hydrated Markdown bodies, with sensitivity/trust filters before agent use.
5. **Knowledge bundles** — introduce an OKF-inspired, file-native
   `index.md`/`log.md` bundle pattern for agent memory, source catalogs, and
   workflow handoff.
6. **Security labels and quarantine follow-up** — persist trust/sensitivity
   metadata for generated, external, private, and reviewed content before broad
   MCP exposure. Search results now expose derived read-safety labels; this
   follow-up should make them policy-backed and user-editable.
7. **Read-first MCP server** — expose safe search, locate, status, inspect, and
   diff tools. Keep push/write operations approval-gated.
8. **Template/workflow store follow-up** — the local pack foundation exists.
   Next: add install-from-git, pack checksums/signatures, desktop gallery, and
   marketplace index metadata.

Actionable prompts from manual E2E testing (June 2026). Each prompt is self-contained for a Codex session. Read `plan.md` and `docs/` before starting any task.

Repo: `/Users/saga4/orgs/research/afs` (AgentFS). Workspace crates: `afs-core`, `afs-store`, `afs-cli`, `afsd`, `afs-notion`, `afs-connector`.

---

## Prompt 1: Fix Notion block push — implement MoveBlock + preflight unsupported ops

### Context from manual testing

A real Notion mount worked for pull/hydrate, but push failed on a simple edit:

- Mount: `afs mount notion ~/afs/notion --root-page 37b3ac0ebb88802cbcf4d53c9cfc4972`
- User added paragraph `"Just Testing 101"` mid-page in `initial-idea/page.md`
- `afs diff` planned: **1 append_block, 6 move_block, 0 update**
- `afs push -y` failed with journal status `failed`
- `afs log` failure message: **`unsupported feature: moving Notion blocks`**
- File stuck **dirty** with **2 failed journals**; `afs pull` correctly skipped dirty file

Root cause: `crates/afs-notion/src/apply.rs` rejects `PushOperation::MoveBlock` in `unsupported_operation_name()`. The diff engine in `crates/afs-core/src/diff.rs` correctly emits moves when inserting content shifts block indices (especially directives). Apply is not implemented.

Secondary UX bug: push journals the plan **before** discovering unsupported ops at apply time. `afs diff` returns `confirm_plan` / `proceed` even though apply will fail.

### Task

1. **Implement `MoveBlock` in `afs-notion` apply path**
   - Use Notion API block reposition (PATCH block with parent/`after` as appropriate for API version `2026-03-11` in `client.rs`)
   - Add `NotionApi` method if needed; implement in `HttpNotionApi` and test fakes
   - Handle chained moves in one push plan (operation order matters)
   - Record journal apply effects for moved blocks

2. **Add connector capability preflight before journaling**
   - Before `execute_journaled_push` writes journal, validate all `PushOperation` variants are supported by the active connector
   - Return structured failure at diff/push planning stage when unsupported (new action e.g. `unsupported_operations`)
   - Include list of unsupported op kinds and `suggested_fix` in JSON output

3. **Add regression tests**
   - Unit test: mid-page insert produces append + moves; apply succeeds with fake API recording PATCH calls
   - Integration-style test mirroring `crates/afs-cli/tests/pull.rs` pattern
   - Golden case: insert one paragraph between existing blocks on a page with multiple blocks/directives

4. **Improve human push failure output**
   - When `apply_failed`, print the journal failure message (not just `push stopped: apply_failed`)

### Acceptance criteria

```bash
# After editing mid-page with one new paragraph:
afs diff <page.md> --json   # may include moves, but no surprise at push
afs push <page.md> -y       # exits 0, action reconciled
afs status <mount>          # clean (no dirty, no failed_journal)
```

Notion page shows the new paragraph in correct position.

### Key files

- `crates/afs-notion/src/apply.rs` — MoveBlock stub at ~1595
- `crates/afs-notion/src/client.rs` — HTTP client
- `crates/afs-core/src/diff.rs` — `should_move_block`, `plan_block_diff`
- `crates/afs-core/src/push.rs` — pipeline + journaled execution
- `crates/afs-cli/src/push.rs`, `crates/afsd/src/push.rs` — CLI/daemon wiring

### Constraints

- Minimize scope; do not rewrite the diff engine unless required for correctness
- Follow existing error/JSON patterns in `docs/cli.md`
- Run `cargo test --workspace` and `cargo fmt --all -- --check`

---

## Prompt 2: Recovery UX — dirty state, failed journals, and better status output

### Context from manual testing

After failed push, user was stuck:

- `afs status` human output only shows: `failed_journal notion-main initial-idea/page.md`
- Does **not** print `issues[]` details (dirty reason, journal failure text) — details only in `--json`
- `afs pull` skips dirty files (correct) but there is **no `--discard-local`** or restore command
- No way to reset without manually editing the file back to shadow content
- `failed_journal` line_state hides that entity is also `dirty`

### Task

1. **Enhance human `afs status` output**
   - For each non-clean entry, print: state, hydration, and each issue `code: message`
   - If `failed_journal_count > 0`, include latest journal failure message from store (query most recent failed journal for entity)
   - Keep compact; one entry block per file

2. **Add `afs restore <path>`** (or `afs pull <path> --discard-local`)
   - Load entity + shadow from store
   - Overwrite local file with last synced canonical render (frontmatter + shadow body)
   - Reset hydration state from `dirty` → `hydrated` when restore succeeds
   - Refuse if `conflicted` without `--force` flag
   - Does **not** call Notion API (local reset to last known good sync)

3. **Document exit codes and JSON shape** in `docs/cli.md`

### Acceptance criteria

```bash
afs status ~/afs/notion
# shows: dirty, failed_journal, and "unsupported feature: moving Notion blocks"

afs restore ~/afs/notion/initial-idea/page.md
afs status ~/afs/notion
# clean (failed journals may still list in log but not block restore)
```

### Key files

- `crates/afs-cli/src/status.rs` — `print_status_report` in `commands.rs`
- `crates/afs-cli/src/commands.rs`
- `crates/afs-store` — journal queries
- `crates/afs-core/src/canonical.rs` — render from shadow

---

## Prompt 3: Daemon reliability — foreground service, health check, and operator UX

### Context from manual testing

- `afsd` runs **foreground only** (blocks terminal); user hit Ctrl-C to stop it
- No `afs daemon status` / `afs daemon start` commands
- Checking if daemon is running requires `pgrep -fl afsd` or probing `~/.afs/afsd.sock` manually
- `afs pull` / `afs push` silently fall back to in-process execution when daemon unavailable (`AFS_DAEMON_DISABLE` or socket missing) — user may not know which path ran
- Daemon supports `DaemonRequest::Ping` over Unix socket but no CLI exposes it
- Recent commits added runtime loop, file watcher, stub-read hydration — needs hardening

### Task

1. **Add `afs daemon status`**
   - Check socket at `$AFS_STATE_DIR/afsd.sock` (default `~/.afs`)
   - Send `Ping` IPC request; report running/stopped + pid if discoverable
   - JSON: `{ "running": true, "socket": "...", "ping": "ok" }`

2. **Indicate execution path in pull/push output**
   - When daemon handled request: include `"via": "daemon"` in JSON; human line `via daemon`
   - When fallback: `"via": "cli"` and stderr hint if socket missing: `afsd not running; using direct execution`

3. **Optional: `afs daemon run` alias** for `afsd` binary with startup banner showing socket path and watched mounts

4. **Fix/strengthen daemon edge cases found in testing**
   - Ensure `NOTION_TOKEN` is required in daemon process env (document clearly)
   - On daemon start, log mount count and roots to stderr
   - Stale socket cleanup already in `server.rs` — add test

5. **Add `docs/daemon.md` section**: operator guide (start, status, stop, troubleshoot)

### Acceptance criteria

```bash
afsd &   # or separate terminal
afs daemon status        # running: true, ping ok
afs pull ~/afs/notion    # reports via daemon
pkill afsd
afs daemon status        # running: false
afs pull ~/afs/notion    # reports via cli + hint to start afsd
```

### Key files

- `crates/afsd/src/ipc.rs` — Ping, socket_path
- `crates/afsd/src/server.rs`, `runtime.rs`
- `crates/afs-cli/src/commands.rs` — `run_daemon_report`, fallback logic
- `docs/daemon.md`

### Out of scope for this prompt

- launchd/systemd background service install
- Automatic daemon spawn from CLI

---

## Prompt 4: Auth Phase 1 — `afs connect notion` + connections table (local, no OAuth yet)

### Context

Current auth is **env-var only**:

- `crates/afs-notion/src/client.rs:173` reads `NOTION_TOKEN` (or `NotionConfig.token_key`)
- `afs connect notion` is a **stub** in `crates/afs-cli/src/commands.rs`
- `MountConfig` has `connector: String` but no `connection_id`
- CLI and daemon both call `default_notion_connector()` — no credential store
- Manual dev flow: `export NOTION_TOKEN=...` then mount/pull/push

### Product model (authoritative — implement toward this, OAuth later)

Three separate concepts:

1. **`afs login`** — optional AgentFS **cloud** identity (relay/team/billing). **NOT required for local v1.** Defer.
2. **`afs connect notion`** — required **provider connection**. Stores credential in OS keychain; metadata in SQLite.
3. **`afs mount notion ...`** — local projection referencing a `connection_id`, not storing secrets.

**Do NOT require global `afs login` for local Notion mounts.**

### Task — Phase 1 only (token connect, no browser OAuth)

1. **Schema: `connections` table** in `afs-store` SQLite (migration):

```sql
CREATE TABLE connections (
  connection_id TEXT PRIMARY KEY,
  connector TEXT NOT NULL,
  display_name TEXT NOT NULL,
  account_label TEXT,
  workspace_id TEXT,
  workspace_name TEXT,
  auth_kind TEXT NOT NULL,       -- oauth, token, env, relay
  secret_ref TEXT NOT NULL,      -- keychain key, NOT the token
  scopes_json TEXT NOT NULL,
  capabilities_json TEXT NOT NULL,
  status TEXT NOT NULL,          -- active, reauth_required, revoked
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  expires_at TEXT
);
ALTER TABLE mounts ADD COLUMN connection_id TEXT;
```

2. **Types**: `ConnectionId`, `ConnectionRecord`; extend `MountConfig` with `connection_id: Option<ConnectionId>`

3. **Credential store abstraction**
   - Trait: `CredentialStore { put(secret_ref, secret), get(secret_ref), delete(secret_ref) }`
   - macOS: Keychain implementation (service `afs`, account `connection:<id>`)
   - Dev fallback: file in `AFS_STATE_DIR` with mode 0600 + warning log (for Linux CI)

4. **CLI commands**
   - `afs connect notion [--name <id>] [--token-stdin]` — read token, probe Notion API, store in keychain, insert connection row
   - `afs connections` — list connections
   - `afs connection show <id>` — metadata only, no secrets
   - `afs disconnect <id>` — revoke row + delete keychain entry

5. **Mount wiring**
   - `afs mount notion <path> --root-page <id> [--connection <id>]`
   - If exactly one active Notion connection, auto-select; if zero, error with `suggested_command: afs connect notion`
   - Save `connection_id` on mount record

6. **Connector resolution**
   - Replace `default_notion_connector()` with resolver: `mount.connection_id → keychain token → NotionConnector`
   - Use in CLI fallback path AND `afsd/runtime.rs`
   - Keep `NOTION_TOKEN` env as deprecated fallback when no connection_id (log warning once)

7. **Auth error contract** — stable codes with `suggested_command`:
   - `missing_connection`, `auth_required`, `credential_store_unavailable`, `connection_revoked`

8. **Tests**
   - SQLite migration test for connections table
   - In-memory credential store for tests
   - `afs connect notion --token-stdin` E2E with fake Notion API probe
   - Mount fails without connection; succeeds after connect

### Acceptance criteria

```bash
unset NOTION_TOKEN
afs mount notion ~/afs/notion --root-page <id>
# → error: missing_connection, suggested_command: afs connect notion

echo "secret_..." | afs connect notion --token-stdin --name work
# → connected notion workspace "..." connection: work

afs mount notion ~/afs/notion --root-page <id> --connection work
afs pull ~/afs/notion    # works without NOTION_TOKEN in env
```

Secrets never appear in `afs connections --json`, logs, or SQLite rows.

### Key files

- `crates/afs-store/src/sqlite.rs` — schema + migrations
- `crates/afs-store/src/records.rs` — MountConfig
- `crates/afs-notion/src/client.rs` — accept token from config, not only env
- `crates/afs-cli/src/commands.rs` — connect stub → real impl
- `crates/afsd/src/runtime.rs` — connector resolution
- New: `crates/afs-auth/` or `crates/afs-store/src/credentials.rs` (follow repo conventions)

### Out of scope (Phase 2 — separate prompt later)

- Browser OAuth flow
- `afs login` / relay auth
- `afs reauth`

---

## Prompt 5: E2E test harness — reproduce the manual test as automated regression

### Context

Manual test path that should become CI/canary:

1. `afs mount notion ~/afs/notion --root-page <uuid>`
2. `afsd` running with `NOTION_TOKEN`
3. `afs pull ~/afs/notion` — stubs + hydrate root
4. Edit hydrated page: insert paragraph mid-body
5. `afs diff` — expect append (+ possibly moves)
6. `afs push -y` — must reconcile
7. `afs status` — clean

Currently no CLI E2E covers this with fake Notion API. `tests/simulation/README.md` describes desired harness.

### Task

1. Extend fake `NotionApi` in tests to support block move PATCH, append, update
2. Add `crates/afs-cli/tests/e2e_push.rs` (or extend existing) covering mount → pull → edit → diff → push → status
3. Add daemon IPC variant test in `crates/afsd/tests/`
4. Mark live Notion test `#[ignore]` with `NOTION_TOKEN` + `AFS_NOTION_PAGE_ID`

### Acceptance

`cargo test --workspace` passes without network secrets. Ignored live test documented in `docs/cli.md`.

---

## Suggested execution order

1. **Prompt 1** (MoveBlock + preflight) — unblocks real editing
2. **Prompt 2** (recovery UX) — unblocks developers when push fails
3. **Prompt 4** (auth Phase 1) — removes NOTION_TOKEN friction
4. **Prompt 3** (daemon UX) — operator clarity
5. **Prompt 5** (E2E harness) — prevent regressions

---

## Manual test environment reference

```bash
export PATH="/path/to/afs/target/debug:$PATH"
export NOTION_TOKEN="..."   # until Prompt 4 lands
afs mount notion ~/afs/notion --root-page <notion-page-uuid>
afsd   # foreground, separate terminal
afs pull ~/afs/notion
# edit initial-idea/page.md
afs diff ~/afs/notion/<page-dir>/page.md --json
afs push ~/afs/notion/<page-dir>/page.md -y
afs status ~/afs/notion --json
afs log
```

State dir: `~/.afs/` (SQLite + `afsd.sock`). Override with `AFS_STATE_DIR`.
