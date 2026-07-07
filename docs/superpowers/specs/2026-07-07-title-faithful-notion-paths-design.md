# Title-Faithful Notion Paths Design

## Goal

Notion pages, database rows, and database directories should project to local
filesystem paths that preserve the remote title as closely as the local
filesystem allows. A remote page titled `Cycle Planning` should appear as
`Cycle Planning/page.md`, not `cycle-planning/page.md`.

## Requirements

- Preserve spaces, casing, punctuation, and Unicode in projected Notion path
  segments when those characters are representable as a local filename.
- Replace only characters that cannot safely live in a path segment: `/`, `\`,
  NUL, and ASCII control characters.
- Trim leading/trailing whitespace and trailing dots from the projected segment
  so paths are usable across the supported projection modes.
- Fall back to `Untitled` when the title has no representable filename content.
- Keep remote IDs as identity. A path change caused by a title or projection
  policy change is still a projection change, not an identity change.
- Keep existing sibling collision behavior: when page/database siblings would
  reserve the same path, all colliding siblings receive a short remote ID suffix.
- Preserve the `page.md` page-document model and existing legacy `.md`
  reservation used to prevent page-directory/file collisions.

## Design

The Notion projection layer already routes page, row, database, workspace, and
lazy virtual listing paths through `allocate_page_path`,
`allocate_directory_path`, and `allocate_child_paths` in
`crates/locality-notion/src/projection.rs`. Replace the current slug stem helper
with a title-faithful stem helper at that boundary.

The helper should:

1. Iterate the Notion title as Unicode scalar values.
2. Preserve normal characters exactly.
3. Replace invalid path-segment characters with a single visible separator,
   using `-` for each invalid run.
4. Trim surrounding whitespace and trailing dots after replacement.
5. Return `Untitled` when the resulting segment is empty.

Collision handling should continue to work on the resulting stem. For example,
two sibling pages both titled `Notes` should project to
`Notes bbbbbb/page.md` and `Notes cccccc/page.md`; a page titled `Notes` and a
database titled `Notes` should also both receive suffixes. Titles that differ
only by now-preserved punctuation or casing should no longer collide unless the
filesystem treats them as the same path through the existing path set.

## Data Flow

Remote Notion titles are read through the existing `page_title` and
`database_title` functions. The projection allocator transforms each title into
one path segment, reserves the page directory, page document, and legacy `.md`
path for pages, then stores the final `TreeEntry.path`. Pull, virtual listing,
hydration, search, status, diff, push, and reconciliation continue to consume
the stored projected paths through the existing repository traits.

## Compatibility

No SQLite schema migration is required. The next enumeration or pull can update
entity paths from old slugified paths to title-faithful paths using existing
remote-ID identity matching and projection reconciliation. Dirty local files
remain protected by the existing pull and push guardrails.

## Testing

- Add unit tests in `crates/locality-notion/src/projection.rs` for title-faithful
  stems, invalid-character replacement, `Untitled` fallback, and suffix
  collisions.
- Update connector enumeration tests in
  `crates/locality-notion/tests/fetch_render.rs` to expect title-faithful
  paths.
- Update CLI pull/projection tests that assert slugified paths so they expect
  `Roadmap`, `Design Notes`, `Fix login bug`, and equivalent title-faithful row
  paths.
- Update `docs/notion-connector.md` and `docs/filesystem-model.mdx` so product
  documentation describes title-faithful paths instead of slugified paths.
