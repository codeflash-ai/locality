# CLI Surface

The `afs` command is the single supported control surface for users and coding agents.

## Commands

- `afs connect notion`
- `afs mount notion [--read-only]`
- `afs status [path] [--json]`
- `afs pull [path] [--json]`
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

Remaining categories to assign before `afs push` applies remote mutations:

- conflict;
- guardrail confirmation required;
- remote concurrency failure.

## Initial `afs diff --json` Shape

The first diff implementation resolves a path through the store, reads the canonical Markdown file, loads its shadow snapshot, and returns the core push-pipeline decision without applying anything. The JSON report includes:

- `validation`: machine-readable issues with file, line, message, and suggested fix;
- `plan.summary`: block/entity/property counts;
- `plan.operations`: connector-neutral planned mutations;
- `plan.degradations`: explicit fidelity warnings from the diff planner;
- `guardrail`: `proceed` or `confirm_required`;
- `action`: the next push action, such as `noop`, `confirm_plan`, `confirm_dangerous_plan`, or `fix_validation`.

The production command path uses the SQLite store. A real diff requires persisted mount, entity, and shadow rows for the target path.
