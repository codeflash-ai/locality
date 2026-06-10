# CLI Surface

The `afs` command is the single supported control surface for users and coding agents.

## Commands

- `afs connect notion`
- `afs mount notion <path> --root-page <page-id> [--mount-id <id>] [--read-only]`
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

`afs mount notion <path> --root-page <page-id>` creates the local root directory and stores a mount record in SQLite. The current auth path is still developer-oriented: the Notion connector reads its bearer token from `NOTION_TOKEN` until OAuth/keychain support is implemented.

`afs pull <mount-root>` enumerates the configured Notion root page, writes stub Markdown files for projected pages, creates directories for projected databases, writes database `_schema.yaml` files, enumerates database row stubs with property frontmatter, hydrates the root page, and persists the root page shadow snapshot. `afs pull <page-file>` hydrates one known entity. Pull refuses to overwrite a hydrated file if its body no longer matches the stored shadow, returning a dirty skip instead.

The JSON report includes `enumerated`, `stubbed`, `hydrated`, and `skipped_dirty` counts.

## Initial `afs diff --json` Shape

The first diff implementation resolves a path through the store, reads the canonical Markdown file, loads its shadow snapshot, and returns the core push-pipeline decision without applying anything. The JSON report includes:

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

Reports also include `push_id`, `journal_status`, changed/reconciled remote IDs, and `apply_effect_count` when execution starts. The Notion connector still reports `apply_not_implemented` because its API mutation methods are stubs, but the CLI now writes a journal first and marks it failed if a connector boundary returns `NotImplemented`.

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
