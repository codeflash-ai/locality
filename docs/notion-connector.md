# Notion Connector

`afs-notion` owns Notion API transport, DTOs, pagination, block rendering, future parse/apply behavior, and Notion-specific error mapping.

## Current Scope

The current implementation is a live-capable read, pull, and narrow write projection:

- `HttpNotionApi` calls the live Notion REST API with a bearer token from `NOTION_TOKEN`.
- `search_pages` can enumerate all pages shared with the integration when no root page is configured.
- root-page enumeration walks child-page and child-database blocks and projects the tree into stable AgentFS paths.
- child databases retrieve their data sources, write `_schema.yaml`, and enumerate row pages under the database directory.
- database row stubs carry the row properties in YAML frontmatter before the body is hydrated.
- `fetch` retrieves page metadata and recursively retrieves paginated block children.
- fetched pages are serialized into a versioned native JSON bundle inside `NativeEntity.raw`;
- `render_native_entity` converts that native bundle into canonical Markdown plus a `ShadowDocument`;
- rich text annotations, external links, date/page/database mentions, link previews, and inline
  equations render to Markdown where there is a stable textual representation;
- simple Notion tables render as Markdown tables with table-row IDs retained in shadow metadata;
- unsupported or lossy blocks render as `::afs{...}` directives so they retain remote identity.
- `afs push -y` can update, append, and archive simple Notion blocks through the live API, then
  reconcile by reading the changed page back into the local shadow.

The generic connector `render` method still returns only `CanonicalDocument`. The Notion connector exposes `render_native_entity` for callers that need the shadow in the same pass. A future connector SDK revision can lift that richer return type into the generic trait once another connector validates the shape.

## Live Test Hook

To test against a real page:

```sh
export NOTION_TOKEN='secret_...'
export AFS_NOTION_PAGE_ID='...'
cargo test -p afs-notion live_fetch_and_render_page_from_environment -- --ignored
```

The token must have access to the target page. Live tests are ignored by default; fixture-backed tests cover normal CI.

## Initial Block Rendering

The renderer currently supports paragraphs, headings, bulleted/numbered list items, to-dos, quotes, callouts, code blocks, simple tables, dividers, child-page/database directives, and unsupported-block directives.

Inline rich text is represented with Notion DTOs first, then rendered through one Markdown path:

- `RichTextDto` mirrors Notion's `text`, `mention`, and `equation` variants plus shared annotations and links.
- `TextRichTextDto`, `MentionRichTextDto`, and `EquationRichTextDto` keep variant-specific payloads out of renderer control flow.
- The renderer preserves whitespace around annotated spans so text like ` bold ` becomes ` **bold** ` instead of pulling spaces into Markdown delimiters.
- Page and database mentions render as `afs://...` links for now. That keeps the remote identity visible until local cross-document link resolution is implemented.
- Unknown or partially populated rich text falls back to `plain_text` so live API additions remain readable.

Nested children are fetched recursively and rendered after their parent, except valid table rows, which are folded into their parent table's Markdown block. This preserves content and block IDs for the first read path, but it does not yet preserve every Notion nesting/layout nuance. Layout-rich blocks should stay directive-backed until the renderer can round-trip them safely.

## Write MVP

The first Notion apply path is intentionally conservative:

- supported operations: block update, block append, and block archive;
- supported writable block forms: paragraphs, headings 1-3, bulleted list items, numbered list items, to-dos, quotes, code fences, and dividers;
- supported rich-text spans: bold, italic, strikethrough, underline, code, external links, inline equations, `afs://` page links, and unchanged preimage mentions such as dates;
- unsupported write forms fail before API mutation, including tables, page/database creation, database row creation, property edits, block moves, and rich inline shapes that cannot be represented by the current Markdown parser;
- appends use Notion's current position object, with `start` for prepends and `after_block` for inserts after a known block;
- before apply, the connector re-reads the page and compares the current Notion edit timestamp against the last-synced timestamp carried by the push executor;
- after apply, the CLI reconciler fetches the changed page, rewrites the local file atomically, saves the refreshed shadow, and updates the entity's `remote_edited_at`.

This gives the end-to-end write loop while preserving the rich inline shapes that the renderer emits. The next fidelity step is widening the inline parser to cover additional mention types, nested annotation/link combinations, and relative-file link resolution.

## Path Projection

Root-page mounts use the same filename shape described in `plan.md`: `slugified-title ~shortid.md`.

For a page that also has child pages, AgentFS reserves a sibling directory with the same stem:

```text
roadmap ~aaaaaa.md
roadmap ~aaaaaa/
  design-notes ~bbbbbb.md
  tasks ~cccccc/
```

The remote ID remains the identity. The short ID starts at six hex characters and lengthens on sibling collisions, while the title slug can change without changing identity.

Database blocks project as directories. Each data source under the database contributes row pages directly inside that directory, and `_schema.yaml` mirrors the current property schema with stable property IDs, types, and select/status option names.

```text
roadmap ~aaaaaa/
  tasks ~cccccc/
    _schema.yaml
    fix-login-bug ~eeeeee.md
```

Row files are normal page files. Their stubs include page identity plus supported property values in frontmatter, while the body remains the standard AFS stub marker until hydration. `_view.csv`, row creation, and property writes remain future work.
