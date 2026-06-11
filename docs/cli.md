# CLI Surface

The `afs` command is the single supported control surface for users and coding agents.

## Commands

- `afs connect notion`
- `afs mount notion <path> --root-page <page-id> [--mount-id <id>] [--read-only]`
- `afs info [path] [--json]`
- `afs status [path] [--json]`
- `afs pull <path> [--json]`
- `afs push [path] [-y|--yes] [--confirm] [--json]`
- `afs diff [path] [--json]`
- `afs undo [push-id] [--json]`
- `afs log [path] [--json]`
- `afs resolve --ours|--theirs|--edited <path>`
- `afs config set <key=value>`

## Exit-code contract

Initial numeric assignments:

- `0`: success;
- `1`: internal, I/O, store, connector, auth, or rate-limit failure;
- `2`: usage error;
- `3`: validation error.
- `4`: confirmation, guardrail, or read-only policy required;
- `5`: command reached an intentionally unimplemented implementation boundary.

Remaining categories to assign before `afs push` applies remote mutations:

- conflict;
- remote concurrency failure.

## Initial `afs mount` and `afs pull`

`afs mount notion <path> --root-page <page-id>` creates the local root directory, writes concise source-specific mount guidance to `AGENTS.md`, creates a `CLAUDE.md` alias for agents that read that filename, and stores a mount record in SQLite. Existing guidance files are preserved. The current auth path is still developer-oriented: the Notion connector reads its bearer token from `NOTION_TOKEN` until OAuth/keychain support is implemented.

`afs pull <mount-root>` enumerates the configured Notion root page, writes stub Markdown files for projected pages, creates directories for projected databases, writes database `_schema.yaml` files, enumerates database row stubs with property frontmatter, hydrates the root page, downloads image media under `media/`, and persists the root page shadow snapshot. `afs pull <page-file>` hydrates one known entity and downloads its image media. Pull refuses to overwrite a hydrated file if its body no longer matches the stored shadow, returning a dirty skip instead.

The JSON report includes `enumerated`, `stubbed`, `hydrated`, and `skipped_dirty` counts.

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
- `reconciled`;
- `apply_not_implemented`;
- `apply_failed`.

Reports also include `push_id`, `journal_status`, changed/reconciled remote IDs, and `apply_effect_count` when execution starts. The Notion connector now applies the supported block and page-property write subset plus new database-row creation through the live API; unsupported connector boundaries still return `apply_not_implemented` or `apply_failed` after the journal is marked failed.

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
