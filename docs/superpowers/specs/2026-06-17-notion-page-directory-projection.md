# Notion Page Directory Projection

## Context

The current Notion projection represents a page as a Markdown file and also
reserves a same-stem directory for possible child content:

```text
roadmap.md
roadmap/
  design-notes.md
  tasks/
```

That shape mirrors the implementation, but it is confusing in the user-facing
filesystem. A human or agent sees two entries for the same Notion page and has
to infer that the `.md` file is the page body while the sibling directory is the
child container. It also doubles directory entries for pages in virtual
projections, because every page can have children and the provider exposes both
objects eagerly.

## Recommendation

Move Notion pages to a directory-backed page layout:

```text
roadmap/
  page.md
  design-notes/
    page.md
  tasks/
    _schema.yaml
    fix-login-bug/
      page.md
```

`page.md` is the canonical Markdown document for the Notion page. Everything
else in that directory is child content of the page: subpages, child databases,
schemas, and future page-local assets.

Use this layout for all Notion page entities in new mounts and after explicit
migration. The all-page rule is simpler than promoting only pages with known
children, avoids path churn when a leaf page later gains children, and matches
the virtual filesystem requirement that every page must be browsable lazily as a
potential container.

Keep database objects as structural directories. A database directory owns
`_schema.yaml` and row pages. Row pages are still Notion pages, so their durable
projection should also become `<row-stem>/page.md`. For creation ergonomics,
continue accepting a new `.md` file written directly inside a database directory
as a pending row create, then reconcile it to the canonical row directory after
Notion assigns the remote page ID.

## Why `page.md`

`page.md` is the best default page-document name because it is explicit for
human users and agents: in a Notion page directory, edit `page.md` for the page
body. It is less tied to web/docs tooling than `index.md`, but the extra
clarity is worth it for a filesystem that non-developers and agents both need
to navigate.

Alternatives considered:

- `README.md`: familiar, but too instruction-like for agent workflows. Notion
  content is untrusted remote data, and naming every page `README.md` invites
  accidental treatment as repository guidance.
- `_page.md`: explicit, but looks internal or generated and does not line up
  with common docs conventions.
- `index.md`: standard in docs tooling, but more developer-oriented and less
  obvious to non-developers.
- `<same-stem>.md` inside the directory: human-readable, but repeats the title
  and makes renames more complex.

Agent guidance should state the rule directly: in a Notion page directory, edit
`page.md`; sibling entries are child Notion content.

## Implementation Plan

### 1. Add a versioned path layout

Add a durable path-layout flag separate from `ProjectionMode`, for example:

```rust
enum NotionPathLayout {
    LegacySibling,
    PageDirectoryIndex,
}
```

Existing mounts keep `LegacySibling` until migrated. New Notion mounts should
default to `PageDirectoryIndex` once the migration and compatibility layer are
ready. Plain-file, macOS File Provider, and Linux FUSE projections should all
use the same layout flag so the user-facing tree is consistent.

### 2. Centralize page path helpers

Stop deriving page containers with ad hoc `with_extension("")`. Add helpers
used by projection, virtual FS, search, pull, push, and tests:

- `page_document_path(stem) -> stem/page.md` for the new layout.
- `page_container_path(page_document_path) -> stem`.
- `page_listing_parent_path(page_document_path) -> parent-of-stem`.
- `is_page_index_path(path)`.
- legacy equivalents for old mounts.

This is the key technical change. Today, many behaviors assume
`EntityRecord.path` was the visible Markdown document and `path.with_extension("")` was
the child container. In the new layout, `EntityRecord.path` is the actual
`page.md` document path, while the visible page item in directory listings is
its parent folder.

### 3. Update Notion projection

Change `locality-notion` path allocation so page entries use
`<slug>/page.md` for clean sibling names and `<slug shortid>/page.md` only
when a sibling collision requires a remote-ID suffix. Reserve both the directory stem and the legacy
`<stem>.md` sibling while allocating names, so the new layout cannot collide
with files from older state or local creates.

Database paths remain directory paths. Database row entries use page document
paths under the database directory.

### 4. Update virtual filesystem semantics

Represent a Notion page as one folder item in its parent listing. Inside that
folder, synthesize a `page.md` file item for the page body, then list child
pages and databases beside it.

Suggested identifiers:

- Page folder: the page remote ID, same as today.
- Page document: `page-document:<remote-id>`.
- Legacy child container aliases: keep accepting `children:<remote-id>` for one
  compatibility window, but do not emit it in listings.

Materialize, write, status, diff, inspect, and push should map the page-document
identifier to the underlying page entity. Renaming the page folder should rename
the Notion page title/stem. Renaming `page.md` should be rejected initially.
Creating `Some Page/New child.md` should create a child page under that page.
Creating `Database/New row.md` should keep the existing pending database-row
create behavior.

### 5. Update plain-file pull and hydration

Plain-file pulls should write stubs and hydrated page bodies at
`<stem>/page.md`. Hydration cache paths for virtual projections should use the
same relative path under the daemon content root.

The pull rename path must migrate a clean legacy pair like this:

```text
old: roadmap.md
old: roadmap/

new: roadmap/page.md
```

If `page.md` already exists, stop with a structured conflict instead of
guessing. If the page is dirty, conflicted, or has pending virtual mutations,
block automatic migration and ask the user to push, restore, or resolve first.

### 6. Add a migration command/path

Add an explicit migration path, likely `loc migrate notion-page-layout <mount>`,
before flipping defaults. The migration should:

- scan entities, virtual mutations, hydration jobs, freshness observations, and
  cached content paths;
- move clean legacy sibling Markdown files into `page.md`;
- update entity paths and path-indexed records in one store transaction after
  filesystem moves succeed;
- write a rollback journal or backup manifest for moved paths;
- skip or fail safely on dirty/conflicted files, existing `page.md` collisions,
  and pending creates/renames/deletes.

Keep old-path lookup aliases for at least one release where practical, so
`loc search`, `loc info`, and explicit CLI paths can explain that
`roadmap.md` moved to `roadmap/page.md`.

### 7. Update docs and guidance

Update `templates/mount/AGENTS.md`, `docs/notion-connector.md`, `docs/cli.md`,
Linux FUSE docs, and desktop copy that currently says pages are `.md` files.
The new rule should be short and repeated consistently:

```text
Notion pages are directories. Edit page.md for the page body. Other entries in
that directory are child Notion content.
```

### 8. Test coverage

Add focused tests before broader snapshots:

- `locality-notion` path allocation produces `<stem>/page.md` and reserves legacy
  sibling names.
- Virtual FS listings show one folder per page and include `page.md` inside the
  page folder.
- Materializing and writing `page.md` hydrate and dirty the page entity.
- `loc search` returns the `page.md` path for pages.
- `loc status`, `loc diff`, `loc inspect`, `loc pull`, and `loc push` accept
  `page.md` paths.
- Database row creation still accepts a new `.md` file directly under a database
  directory and reconciles to `<row-stem>/page.md`.
- Migration moves a clean legacy sibling pair, blocks dirty/conflicted pages,
  and reports existing `page.md` collisions.
- Linux FUSE and macOS File Provider behavior tests assert no same-stem page
  file plus folder pair appears in a listing.

## Rollout

1. Land helpers, docs, and tests behind a non-default layout flag.
2. Implement new-layout projection and virtual FS behavior.
3. Add migration command and compatibility path explanations.
4. Flip new Notion mounts to `PageDirectoryIndex`.
5. After one compatibility window, remove emitted legacy aliases while keeping
   store migration support for older state.

## Open Questions

- Should the desktop app expose migration as a button when it detects a legacy
  Notion mount, or should migration remain CLI-only for the first release?
- How long should old-path aliases live after migration?
