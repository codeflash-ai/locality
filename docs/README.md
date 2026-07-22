# Locality Internal Docs

This directory contains internal engineering notes, design docs, plans, diagnostics, and implementation references for the Locality repository.

The public Mintlify documentation site lives in `../docs-site/`.

Use this directory for repo-facing documentation that helps contributors understand architecture, sync behavior, platform internals, release implementation, and design history. Use `../docs-site/` for user-facing docs that should be published.

## Architecture entry points

- [`architecture.md`](architecture.md): currently shipped local-first system.
- [`cloud-sandbox-data-plane.md`](cloud-sandbox-data-plane.md): proposed backend
  and client architecture for cloud coding-agent sandboxes.

## Static collaboration artifacts

- `wireframes/index.html`: Locality desktop wireframe deck. If GitHub Pages is configured to serve the repository's `docs/` directory, this is available at `/wireframes/`. Sibling screen URLs such as `/wireframes/home.html` are generated from the root deck with `node scripts/generate-wireframe-pages.mjs`.
- `agent-conversation-profile.md`: usage notes for the local experiment that turns
  Claude/Codex conversation exports into Perfetto-readable traces.
