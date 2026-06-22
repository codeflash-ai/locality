# Overleaf Integration Plan

## Summary

Build Overleaf as a first-party Git-backed connector, not a Notion-style block
API connector. Overleaf's supported integration surface is project Git remotes
with token-based authentication, so v1 should mount one Overleaf project per
local directory and keep project files verbatim: `.tex`, `.bib`, images, style
files, and other repository content. AgentFS should not inject YAML frontmatter
into LaTeX or project files.

## Key Changes

- Add `crates/afs-overleaf` with `OverleafConfig { remote_url, token,
  token_key, branch }`, an `OverleafGitBackend` trait for testable Git
  operations, and a default implementation backed by the system `git` CLI.
- Add CLI support for `afs connect overleaf --token-stdin [--name <id>]` and
  `afs mount overleaf <path> --remote-url <git-url> [--connection <id>]
  [--mount-id <id>] [--branch main] [--read-only]`.
- Extend the daemon source registry with `ResolvedSource::Overleaf`, an
  Overleaf source descriptor, default mount id `overleaf-main`, connector
  guidance, and credential resolution from the existing connection store.
- Treat Overleaf mounts as raw Git project worktrees. `afs pull <path>` should
  run authenticated fetch plus safe fast-forward behavior, and `afs push <path>`
  should stage project changes, create one AFS-generated commit, and push to
  the Overleaf remote.
- Stop push when remote and local changes diverge. Report a clear conflict and
  require the user or agent to resolve via pull/rebase before retrying.
- Leave Notion canonical Markdown behavior unchanged. Overleaf files should not
  be parsed as `CanonicalDocument`, and existing Markdown/frontmatter validation
  should be skipped for `connector == "overleaf"`.

## Public Interface

- New connector id: `overleaf`.
- New environment fallback: `OVERLEAF_GIT_TOKEN`.
- Stored connection records use `connector = "overleaf"` and
  `auth_kind = "git_token"`.
- Capabilities should include project read/write access, represented in
  `capabilities_json` with `read_project` and `write_project`.
- v1 mount requires a project Git URL copied from Overleaf. Workspace or project
  discovery is out of scope.

## Test Plan

- Unit-test Overleaf credential resolution and missing-token errors.
- Unit-test CLI parsing for `connect overleaf` and `mount overleaf`.
- Use a fake `OverleafGitBackend` to test clean fast-forward pull, local edits
  pushed as one commit, read-only push blocking, divergence blocking with
  conflict guidance, and byte-for-byte preservation of binary files.
- Add integration-style tests with a local bare Git repository standing in for
  Overleaf.

## Assumptions

- v1 targets Overleaf project Git sync with Git authentication tokens.
- One Overleaf project maps to one AgentFS mount.
- No OAuth broker is needed for v1.
- Overleaf comments, compile logs, collaborators, history UI, and
  workspace-wide project discovery are out of scope.
