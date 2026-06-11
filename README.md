# AgentFS

AgentFS mounts systems of record as real Markdown files that agents and editors can read, grep, and edit locally. Reads are implicit through the daemon. Writes are explicit by default through `afs push`, which validates, plans, journals, and applies changes back to the source with connector-specific APIs.

This repository contains the Rust workspace for the `plan.md` design and the first functional slices of the core sync engine, CLI, store, daemon hydration loop, and Notion connector.

## Workspace layout

- `crates/afs-cli`: `afs` command surface for humans and agents.
- `crates/afsd`: per-user daemon supervising mounts, watchers, hydration, pull, and push orchestration.
- `crates/afs-core`: connector-agnostic sync engine, three-tree model, diff, planning, conflicts, hydration state, validation, and journal abstractions.
- `crates/afs-connector`: connector SDK trait for enumerate, fetch, render, parse, and apply.
- `crates/afs-notion`: first-party Notion connector with live page/block reads, database row projection, schema rendering, narrow block writes, and supported page-property writes.
- `crates/afs-store`: state-store abstraction and SQLite implementation.
- `templates/mount/AGENTS.md`: generated mount guidance template for coding agents.
- `docs/`: design notes split by implementation surface.

## Current status

The implementation is still early, but the main module boundaries are now exercised end to end: mount writes concise agent guidance, mount and pull can project a Notion root page into files, database rows appear as page stubs with property frontmatter, selected pages can hydrate, info explains local source context, status reports local dirty/stub/conflict state, simple block and supported property edits can push with journaling and reconciliation, and daemon hydration requests have a tested execution path.
