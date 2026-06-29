# Locality Agent Guide

Locality turns remote systems of record, especially Notion, into local files that can
be read, edited, reviewed, and safely pushed back. This repo is organized around
that filesystem contract.

## Product Invariants

- Prefer filesystem semantics over special commands. If creating or editing a
  file naturally expresses the user intent, that should be the product path.
- Notion pages are directories whose body is `page.md`; child pages live as
  child directories. Database rows are page-like entries under database
  directories.
- The containing directory determines the remote parent. Do not infer parents
  from search or titles.
- Preserve existing Locality frontmatter unless the task is explicitly about
  identity, migration, or rendering.

## Working In This Repo

- Read the relevant `docs/` page and nearby tests before changing behavior.
  Important design decisions often live in docs.
- Use existing crate boundaries and repository traits where possible. Avoid
  parallel code paths that make CLI, daemon, desktop, MCP, and File Provider
  behavior drift apart.
- For substantial or risky work, use a separate git worktree. Multiple agents
  often work on this repo at the same time, so preserve unrelated changes and
  never reset or revert work you did not make.
- Major behavior or architecture changes should include a docs update in the
  same change.
- When changing some code, don't merely continue the current design if its not
  a good fit for the proposed changes. Think about what the right design be
  with the new change and redesign the code in a focused manner if pragmatic.

## State And Compatibility

- Treat SQLite state as durable user state, not a cache to reset. Normal
  upgrades should migrate or repair automatically and stay non-user-visible.
- Use `PRAGMA user_version` for physical schema changes. Use component versions
  for semantic changes to persisted formats such as connector state, projection
  layout, journals, virtual mutations, push plans, IPC protocols, or secret
  references.
- Rebuildable data, such as search indexes, hydration queues, freshness state,
  content caches, and File Provider registrations, should have repair paths
  instead of forcing a user reset.
- Compatibility changes need tests that old state opens or migrates, newer state
  fails cleanly with an update-required outcome, and pending local work is not
  silently discarded.

## Live Mode Agent Semantics

- Live Mode is the desktop background sync loop. When it is enabled, safe local
  edits can be pushed and clean remote changes can be pulled without an agent
  manually running `loc pull` or `loc push` after every file edit.
- Agent guidance should tell agents to edit mounted Markdown directly and stop
  unless the user asks for review or push. Use `loc status`/`loc diff` for
  inspection; reserve `loc pull`/`loc push` for explicit requests, recovery, or
  cases where Live Mode reports paused/review-needed/conflicted state.
- Live Mode must stay conservative. It should pause for conflicts, remote drift,
  destructive or large plans, unsupported operations, and anything requiring
  user approval rather than trying to be fully autonomous.

## macOS File Provider Notes

- The CloudStorage-visible file, the File Provider local replica, and daemon
  state can diverge. When debugging writes, verify both the visible file and
  `loc status`/`loc diff`.
- Atomic writes through temp files and renames are normal editor and agent
  behavior; File Provider and daemon paths should support them.
- For File Provider changes, a passing build is not enough. Verify the installed
  app, daemon, and registered domain because macOS can keep old extensions or
  capabilities active.

## Testing Expectations

- Add focused behavior tests for sync, push/pull, Notion rendering, virtual
  mutations, state compatibility, File Provider/FUSE projection, auto-save
  policy, and desktop command flows. Any new behavior requires tests.
- Prefer exact expected output for canonical Markdown, schemas, generated
  guidance, and push plans. Substring assertions have missed regressions here.
- For live Notion verification, use scratch content, verify through the real
  product path, and clean up remote and local artifacts.
- For release or install changes, verify the installed app, sidecars, daemon,
  CLI, mount, and app UI, not just the build artifact.

## Release Discipline

- Sync with `origin/main` before release work unless the user asks for a branch
  build.
- Version bumps, tags, GitHub publish flows, and `make publish` runs should be
  deliberate and user-requested.
- Release paths should be gated by tests. Do not publish a build you know has
  failed the relevant checks.
