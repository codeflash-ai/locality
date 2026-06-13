# Notion Connector

`afs-notion` owns Notion API transport, DTOs, pagination, block rendering, future parse/apply behavior, and Notion-specific error mapping.

## Current Scope

The current implementation is a live-capable read, pull, and narrow write projection:

- `HttpNotionApi` calls the live Notion REST API with a bearer token resolved from an OAuth connection, an explicit PAT connection, or the legacy `NOTION_TOKEN` environment fallback.
- `search_pages` can enumerate all pages shared with the integration when no root page is configured.
- root-page enumeration walks child-page and child-database blocks and projects the tree into stable AgentFS paths.
- child databases retrieve their data sources, write `_schema.yaml`, and enumerate row pages under the database directory.
- database row stubs carry the row properties in YAML frontmatter before the body is hydrated.
- `fetch` retrieves page metadata and recursively retrieves paginated block children.
- fetched pages are serialized into a versioned native JSON bundle inside `NativeEntity.raw`;
- `render_native_entity` converts that native bundle into canonical Markdown plus a `ShadowDocument`;
- rich text annotations, external links, date/page/database mentions, link previews, inline
  equations, display equations, and heading levels 1-4 render to Markdown where there is a
  stable textual representation;
- simple Notion tables render as Markdown tables with table-row IDs retained in shadow metadata;
- toggles, synced blocks, column layouts, tabs, meeting notes,
  AI/custom blocks, URL-less media payloads, and unsupported or lossy blocks render as `::afs{...}` directives so they
  retain remote identity and useful metadata such as title, URL, source block ID, or target page ID
  when the API exposes it.
- bookmark/embed/link-preview URL blocks render as ordinary Markdown links.
- media blocks with a Notion URL render as ordinary Markdown image or link syntax, while still
  keeping local media download metadata in the rendered entity for filesystem-aware callers.
- `afs push -y` can update, append, and archive simple Notion blocks, update supported page
  properties, create new rows in single-data-source databases, and reconcile by reading the changed
  or created page back into the local shadow.
- database row property edits and row creation are validated against the local `_schema.yaml`
  before journal/apply.

The generic connector `render` method still returns only `CanonicalDocument`. The Notion connector exposes `render_native_entity` for callers that need the shadow in the same pass. A future connector SDK revision can lift that richer return type into the generic trait once another connector validates the shape.

## Live Test Hook

The product connection path prefers OAuth:

```sh
afs connect notion --name work
```

The default product path uses the AFS OAuth broker so the local CLI never ships
or stores the Notion OAuth client secret. The broker URL can be overridden with
`--broker-url <url>`, `AFS_NOTION_OAUTH_BROKER_URL`, or `AFS_AUTH_BROKER_URL`.
The Notion public integration must register the callback URI, which defaults to
`http://localhost:8757/oauth/notion/callback`.

For development with a BYO Notion OAuth app, use direct OAuth:

```sh
export AFS_NOTION_OAUTH_CLIENT_ID='...'
export AFS_NOTION_OAUTH_CLIENT_SECRET='...'
afs connect notion --direct-oauth --name work
```

For development and CI, the explicit PAT fallback remains available:

```sh
echo "$NOTION_TOKEN" | afs connect notion --token-stdin --name work
```

To test against a real page:

```sh
export NOTION_TOKEN='secret_...'
export AFS_NOTION_PAGE_ID='...'
cargo test -p afs-notion live_fetch_and_render_page_from_environment -- --ignored
```

The token must have access to the target page. Live tests are ignored by default; fixture-backed tests cover normal CI.

The broader live integrity suite creates and archives scratch content under a writable parent page. `AFS_NOTION_LIVE_PARENT_PAGE` may be a page ID or a Notion page URL. `AFS_NOTION_LIVE_DIR` is optional and controls where local media artifacts are downloaded:

```sh
export NOTION_TOKEN='secret_...'
export AFS_NOTION_LIVE_PARENT_PAGE='https://app.notion.com/...'
export AFS_NOTION_LIVE_DIR=/tmp/afs-notion-live
cargo test -p afs-notion --test live_integrity -- --ignored
```

Those tests cover broad block rendering, supported block edits/appends, image download, database row creation, supported property writes, and read-back verification against the live API. They require the integration to have insert, read, and update content capabilities for the parent page. The current support contract is tracked in [notion-object-support.md](notion-object-support.md).

The product-level mounted workflow test uses the same parent page, but exercises the AFS user path instead of only the connector boundary. It creates a scratch page, mounts it as plain files, pulls it locally, edits the Markdown file, verifies `afs status` reports pending changes, pushes the edit, fetches the page from Notion, and archives the scratch page:

```sh
export NOTION_TOKEN='secret_...'
export AFS_NOTION_LIVE_PARENT_PAGE='https://app.notion.com/...'
cargo test -p afs-cli --test e2e_push_workflow live_scratch_page_mount_edit_push_verifies_notion -- --ignored --exact
```

GitHub Actions has a manual `notion-live-e2e` workflow for these tests. The workflow should be backed by a disposable Notion workspace/account and secrets named `NOTION_TOKEN` and `AFS_NOTION_LIVE_PARENT_PAGE`.

## Initial Block Rendering

The renderer currently supports paragraphs, headings 1-4, bulleted/numbered list items, to-dos, quotes, callouts, code blocks, simple tables, dividers, display equations, bookmark/embed/link-preview URL blocks, and media blocks with URLs as Markdown. It renders child pages/databases, toggles, synced blocks, column layouts, tabs, table of contents, breadcrumbs, meeting notes, AI/custom blocks, URL-less media payloads, and unknown future blocks as anchored directives.

Inline rich text is represented with Notion DTOs first, then rendered through one Markdown path:

- `RichTextDto` mirrors Notion's `text`, `mention`, and `equation` variants plus shared annotations and links.
- `TextRichTextDto`, `MentionRichTextDto`, and `EquationRichTextDto` keep variant-specific payloads out of renderer control flow.
- The renderer preserves whitespace around annotated spans so text like ` bold ` becomes ` **bold** ` instead of pulling spaces into Markdown delimiters.
- Page and database mentions render as normal Markdown links to Notion object URLs. Unchanged mention preimages still preserve typed Notion mentions during block updates, and agents can create or reassert typed links with `@page(<notion-page-id>)` and `@database(<notion-database-id>)`.
- Unknown or partially populated rich text falls back to `plain_text` so live API additions remain readable.

Nested children are fetched recursively and rendered after their parent, except valid table rows, which are folded into their parent table's Markdown block. This preserves content and block IDs for the first read path, but it does not yet preserve every Notion nesting/layout nuance. Layout-rich blocks should stay directive-backed until the renderer can round-trip them safely.

## Write MVP

The first Notion apply path is intentionally conservative:

- supported operations: block update, block append, block archive, supported page property update, and database row creation;
- supported writable block forms: paragraphs, headings 1-4, bulleted list items, numbered list items, to-dos, quotes, callouts, code fences, dividers, display equations, existing stable-width/header-mode tables including row add/delete, existing bookmark/embed URL blocks, and existing URL-backed media blocks;
- supported rich-text spans: bold, italic, strikethrough, underline, code, external links, inline equations, Notion page links, database links whose target ID matches a rendered database mention, explicit `@page(...)` page mentions, explicit `@database(...)` database mentions, explicit `@date(...)` date mentions, explicit `@user(...)` user mentions, legacy `afs://` page links, and unchanged preimage mentions such as dates/users;
- supported page property writes: title, rich text, number, select, status, multi-select, checkbox, date, URL, email, phone, external file URLs, explicit people user IDs, and explicit relation page IDs;
- new row creation accepts a new Markdown file under a projected database directory, uses the file's `title` as the row title, maps supported frontmatter properties through the live data source schema, creates initial children from directly supported Markdown blocks, and then reconciles the created page into its stable `slug ~shortid.md` path;
- unsupported write forms fail before API mutation, including table width or header-mode changes, page/database creation outside database-row files, computed/read-only properties, hosted file uploads/rewrites, multi-data-source row creation, and rich inline shapes that cannot be represented by the current Markdown parser;
- appends use Notion's current position object, with `start` for prepends and `after_block` for inserts after a known block;
- before apply, the connector re-reads the page and compares the current Notion edit timestamp against the last-synced timestamp carried by the push executor;
- after apply, the CLI reconciler fetches changed and created pages, rewrites local files atomically, saves refreshed shadows, updates `remote_edited_at`, and removes the temporary source filename when a created row moves into its projected path.

This gives the end-to-end write loop while preserving the rich inline shapes that the renderer emits. The next fidelity step is widening the inline parser to cover nested annotation/link combinations, local relative-file link resolution, and remaining specialized Notion mention variants.

## Schema-Backed Property Validation

Projected database directories carry `_schema.yaml`, generated from the live Notion database data source schema. Before `afs diff` or daemon-backed `afs push` accepts a database row property change, AFS reads that file and validates the frontmatter keys that would actually be written.

For existing rows, only changed frontmatter properties are validated, so read-only values rendered from Notion, such as formulas or rollups, can remain in the file unchanged. For new row files, every non-identity frontmatter property is validated because all of them become create-page payload fields.

The current validator supports the same writable property set as apply: `title`, `rich_text`, `number`, `select`, `status`, `multi_select`, `checkbox`, `date`, `url`, `email`, `phone_number`, external `files`, explicit `people` user IDs, and `relation` page IDs. Select-like values must use option names already present in `_schema.yaml`; unknown options stop as `fix_validation` instead of implicitly creating new Notion options. Computed, read-only, or unresolved types such as `formula`, `rollup`, timestamps, users, `unique_id`, and `verification` are blocked with structured validation errors until their ownership and resolution policies are designed.

Multi-data-source databases still stop before row writes because AFS does not yet have a path-level way to choose the target data source. Pull the database again if `_schema.yaml` is missing or stale.

## Local Media

When a caller renders a Notion page for a known filesystem path, media blocks with `external.url` or Notion-hosted `file.url` render as Markdown image/link syntax. Image blocks are also downloaded to a mount-level media tree:

```text
media/
  roadmap ~aaaaaa/
    image-0123456789ab.png
```

The media tree mirrors the Markdown page path without the `.md` extension. This keeps binary files out of content directories while giving agents a stable local file they can open. The first downloader fetches image blocks only; other file-like blocks render their remote URL directly until size and retention policy are designed.

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

Row files are normal page files. Their stubs include page identity plus supported property values in frontmatter, while the body remains the standard AFS stub marker until hydration. Creating a row is creating a new `.md` file in the database directory with YAML frontmatter and no `afs.id`; `afs push -y` creates the Notion page, reads it back, saves the durable entity/shadow rows, and replaces the temporary filename with the canonical projected filename. `_view.csv` remains future work.
