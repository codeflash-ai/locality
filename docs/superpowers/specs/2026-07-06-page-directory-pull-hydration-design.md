# Page Directory Pull Hydration Design

## Problem

`loc pull <page-directory>` does not behave consistently today.

- On virtual/File Provider mounts, pulling a page directory hydrates descendant
  `page.md` files in child directories, but leaves the target directory's own
  `page.md` as a stub.
- On plain-files mounts, directory pulls follow a different path entirely, so
  the page-directory target semantics are not explicitly modeled at all.

This creates a product mismatch with the filesystem contract. A page directory
*is* the page, so pulling that directory should hydrate the page body that lives
at `<page-directory>/page.md`, not only its descendants.

## Goal

Make `loc pull <page-directory>` hydrate that directory's own `page.md`
consistently on every mount type, while preserving existing recursive hydration
for child page directories and existing dirty/conflict protections.

## Non-Goals

- Do not change `loc pull <page-file>` semantics.
- Do not change mount-root pull behavior.
- Do not change the database-directory row hydration limit or `_schema.yaml`
  refresh behavior.
- Do not weaken dirty/conflict protection for already-hydrated content.

## Desired Behavior

Treat a page directory as a first-class pull target across projection modes.

When the target path resolves to a page directory:

- hydrate or refresh `<target>/page.md` first;
- then recurse into child page directories below that page;
- report hydration, skipped-dirty, and conflict counts through the existing pull
  report fields; and
- materialize the visible file appropriately for the active projection.

When the target path resolves to a database directory:

- keep the existing row-listing and `_schema.yaml` refresh behavior; and
- keep the existing small-database row hydration behavior.

When the target path resolves to the mount root or a page file:

- keep existing behavior unchanged.

## Design

Add a shared pull target classification step before dispatching to the existing
mount-specific pull logic. The classifier should distinguish:

- mount root;
- page file;
- page directory;
- database directory; and
- other directory targets.

The page-directory branch becomes the common semantic entry point for all mount
types. That branch should:

1. resolve the page entity for `<target>/page.md`;
2. hydrate that page entity with the same `hydrate_entity` path already used for
   file targets;
3. enumerate child entries for that page, persisting refreshed child entity
   records as needed; and
4. recurse into descendant child pages using the existing recursive hydration
   logic.

For virtual mounts, the current `pull_virtual_directory_path` implementation can
be refactored so the target page's remote id is included in the recursive
hydration flow instead of starting one level below the target. The important
requirement is semantic, not whether that happens by queueing the target page id
or by an explicit pre-hydration step.

For plain-files mounts, directory targets should stop behaving like an implicit
mount-root pull when they actually resolve to a page directory. Instead, they
should use the same page-directory target semantics as virtual mounts.

## Implementation Notes

- Prefer a shared resolver in `crates/localityd/src/pull.rs` instead of
  scattering new special cases across CLI and mount-specific branches.
- Reuse `find_entity_by_path`, `page_container_path`, and existing hydration
  helpers rather than adding a second page lookup path.
- Keep database directories and generic directories distinguishable so the
  current database behavior remains intact.
- Preserve report accounting semantics, but adjust expected counts where the
  target page itself now hydrates.

## Testing

Write tests first and watch them fail before changing production code.

Required coverage:

- update the existing virtual pull regression in
  `crates/loc-cli/tests/pull.rs` to expect the target page directory's own
  `page.md` to hydrate;
- update the File Provider-visible regression in
  `crates/loc-cli/tests/pull.rs` to expect the target page's visible `page.md`
  to materialize with content;
- update the cross-projection e2e regression in
  `crates/loc-cli/tests/e2e_push_workflow.rs` to expect the target page itself
  to hydrate across macOS File Provider, Linux FUSE, and Windows Cloud Files;
- add a plain-files regression proving `loc pull <page-directory>` hydrates the
  target page body consistently there too; and
- keep existing database-directory tests green.

## Manual Verification

After the code change:

- rerun the focused pull tests above;
- rerun the cross-projection e2e test;
- verify on the installed macOS File Provider mount that pulling an absolute
  page-directory path marks the target page hydrated and that opening
  `<target>/page.md` through the visible CloudStorage path yields the remote
  body.

## Compatibility

No schema migration is required. This is a pull-target semantic fix that should
make CLI, daemon, and projection behavior more consistent with the existing page
directory filesystem model.
