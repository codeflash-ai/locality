# Notion Connector

`locality-notion` owns Notion API transport, DTOs, pagination, block rendering, future parse/apply behavior, and Notion-specific error mapping.

## Current Scope

The current implementation is a live-capable read, pull, and narrow write projection:

- `HttpNotionApi` calls the live Notion REST API with a bearer token resolved from an OAuth connection, an explicit PAT connection, or the legacy `NOTION_TOKEN` environment fallback.
- `search_pages` can enumerate all pages shared with the integration when no root page is configured.
- root-page enumeration walks child-page and child-database blocks and projects the tree into stable Locality paths.
- pulling a projected page directory in a virtual/File Provider mount recursively enumerates child-page
  blocks below that page, hydrates those child pages, and materializes clean visible replicas when
  the platform projection supports them.
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
  AI/custom blocks, URL-less media payloads, and unsupported or lossy blocks render as `::loc{...}` directives so they
  retain remote identity and useful metadata such as title, URL, source block ID, or target page ID
  when the API exposes it.
- bookmark/embed/link-preview URL blocks render as ordinary Markdown links.
- media blocks with a Notion URL render as ordinary Markdown image or link syntax; filesystem-aware
  media writes use absolute local hrefs under the projection output root and keep mount-relative
  download metadata for pull, hydration, and post-push reconcile.
- `loc push -y` can update, append, and archive simple Notion blocks, upload changed local
  file-like media for existing image/video/file/pdf/audio blocks, append new local media blocks, update supported page
  properties, create new rows in single-data-source databases, and reconcile by reading the changed
  or created page back into the local shadow.
- database row property edits and row creation are validated against the local `_schema.yaml`
  before journal/apply.

The generic connector `render` method still returns only `CanonicalDocument`. The Notion connector exposes `render_native_entity` for callers that need the shadow in the same pass. A future connector SDK revision can lift that richer return type into the generic trait once another connector validates the shape.

## Live Test Hook

The product connection path prefers OAuth:

```sh
loc connect notion --name work
```

The default product path uses the Locality OAuth broker so the local CLI never ships
or stores the Notion OAuth client secret. The broker URL can be overridden with
`--broker-url <url>`, `LOCALITY_NOTION_OAUTH_BROKER_URL`, or `LOCALITY_AUTH_BROKER_URL`.
The Notion public integration must register the callback URI, which defaults to
`http://localhost:8757/oauth/notion/callback`.

For development with a BYO Notion OAuth app, use direct OAuth:

```sh
export LOCALITY_NOTION_OAUTH_CLIENT_ID='...'
export LOCALITY_NOTION_OAUTH_CLIENT_SECRET='...'
loc connect notion --direct-oauth --name work
```

For development and CI, the explicit PAT fallback remains available:

```sh
echo "$NOTION_TOKEN" | loc connect notion --token-stdin --name work
```

To test against a real page:

```sh
export LOCALITY_NOTION_PAGE_ID='...'
cargo test -p locality-notion live_fetch_and_render_page_from_environment -- --ignored
```

`LOCALITY_NOTION_PAGE_ID` may be omitted when
`LOCALITY_NOTION_LIVE_PARENT_PAGE` is already set; the smoke will fetch/render
that page instead. Auth uses `NOTION_TOKEN` when explicitly set, otherwise it
reads the installed Locality credential store for `connection:notion-default`
under `~/.loc/credentials`. The token must have access to the target page. Live
tests are ignored by default; fixture-backed tests cover normal CI.

The broader live integrity suite creates and archives scratch content under a writable parent page. `LOCALITY_NOTION_LIVE_PARENT_PAGE` may be a page ID or a Notion page URL. `LOCALITY_NOTION_LIVE_DIR` is optional and controls where local media artifacts are downloaded:

```sh
export NOTION_TOKEN='secret_...'
export LOCALITY_NOTION_LIVE_PARENT_PAGE='https://app.notion.com/...'
export LOCALITY_NOTION_LIVE_DIR=/tmp/locality-notion-live
cargo test -p locality-notion --test live_integrity -- --ignored
```

Those tests cover broad block rendering, supported block edits/appends, media download, local media upload, database row creation, supported property writes, and read-back verification against the live API. They require the integration to have insert, read, and update content capabilities for the parent page. The current support contract is tracked in [notion-object-support.md](notion-object-support.md).

The product-level mounted workflow test uses the same parent page, but exercises the Locality user path instead of only the connector boundary. It creates a scratch page, mounts it as plain files, pulls it locally, edits the Markdown file, verifies `loc status` reports pending changes, pushes the edit, fetches the page from Notion, and archives the scratch page:

```sh
export LOCALITY_NOTION_LIVE_PARENT_PAGE='https://app.notion.com/...'
cargo test -p loc-cli --test e2e_push_workflow live_scratch_page_mount_edit_push_verifies_notion -- --ignored --exact
```

Those workflow tests use `NOTION_TOKEN` when it is set, otherwise they read the
installed Locality credential store for `connection:notion-default` under
`~/.loc/credentials` on Linux/dev installs. The connector-level live tests use
the same lookup. Set `LOCALITY_NOTION_LIVE_CONNECTION_ID` to use a different
stored Notion connection. The mounted workflow and Linux FUSE live tests preflight
`LOCALITY_NOTION_LIVE_PARENT_PAGE` and fail before scratch creation if it points
to an archived, trashed, inaccessible, or non-page Notion object.

GitHub Actions has a manual and `main`-branch `notion-live-e2e` workflow for
these tests. The workflow should be backed by a disposable Notion
workspace/account and secrets named `NOTION_TOKEN` and
`LOCALITY_NOTION_LIVE_PARENT_PAGE`. The connector, mounted workflow, Linux FUSE,
and Windows Cloud Files jobs seed the stored credential path from `NOTION_TOKEN`
and then run test/product commands with token environment variables removed. The
Windows job also mounts a live Cloud Files sync root and runs `loc doctor
--json` against that live state before exercising provider file operations.

## Initial Block Rendering

The renderer currently supports paragraphs, headings 1-4, bulleted/numbered list items, to-dos, quotes, callouts, code blocks, simple tables, dividers, display equations, bookmark/embed/link-preview URL blocks, child-page links, and media blocks with URLs as Markdown. Filesystem-aware page writes point at the downloaded local media file under the projection output root instead of a transient Notion-hosted URL; for virtual projections this is the daemon content-cache path. Durable shadows keep the connector-rendered mount-relative href so state remains portable. Child pages render as normal Markdown links to their stable Notion page URLs so agents and humans can follow or locate the editable child page's `page.md`. It renders child databases, toggles, synced blocks, column layouts, tabs, table of contents, breadcrumbs, meeting notes, AI/custom blocks, URL-less media payloads, and unknown future blocks as anchored directives.

Inline rich text is represented with Notion DTOs first, then rendered through one Markdown path:

- `RichTextDto` mirrors Notion's `text`, `mention`, and `equation` variants plus shared annotations and links.
- `TextRichTextDto`, `MentionRichTextDto`, and `EquationRichTextDto` keep variant-specific payloads out of renderer control flow.
- The renderer preserves whitespace around annotated spans so text like ` bold ` becomes ` **bold** ` instead of pulling spaces into Markdown delimiters.
- Notion-only annotation state, such as non-default text colors, is not rendered into Markdown, but unchanged rich-text preimages preserve that state when adjacent Markdown text is edited and pushed. Block background colors are likewise preserved for ordinary text edits by sending only the changed editable fields in the Notion block update.
- Page and database mentions render as normal Markdown links to Notion object URLs. Unchanged mention preimages still preserve typed Notion mentions during block updates, and agents can create or reassert typed links with `@page(<notion-page-id>)` and `@database(<notion-database-id>)`.
- Unknown or partially populated rich text falls back to `plain_text` so live API additions remain readable.

Nested children are fetched recursively and rendered after their parent, except valid table rows, which are folded into their parent table's Markdown block. This preserves content and block IDs for the first read path, but it does not yet preserve every Notion nesting/layout nuance. Layout-rich blocks should stay directive-backed until the renderer can round-trip them safely.

## Write MVP

The first Notion apply path is intentionally conservative:

- supported operations: block update, block append, safe childless directive block move, block archive, local file-like media update, supported page property update, and database row creation;
- supported writable block forms: paragraphs, headings 1-4, bulleted list items, numbered list items, to-dos, quotes, callouts, code fences, dividers, display equations, existing stable-width/header-mode tables including cell edits, row appends, and trailing row deletes, existing bookmark/embed URL blocks, existing URL-backed media blocks, existing local image/video/file/pdf/audio media blocks, and new local media block appends;
- supported rich-text spans: bold, italic, strikethrough, underline, code, external links, inline equations, Notion page links, database links whose target ID matches a rendered database mention, explicit `@page(...)` page mentions, explicit `@database(...)` database mentions, explicit `@date(...)` date mentions, explicit `@user(...)` user mentions, legacy `loc://` page links, and unchanged preimage mentions such as dates/users;
- supported page property writes: title, rich text with the same inline Markdown parser used by page bodies, number, select, status, multi-select, checkbox, date, URL, email, phone, external file URLs, explicit people user IDs, and explicit relation page IDs;
- new row creation accepts a new Markdown file under a projected database directory, uses the file's `title` as the row title, maps supported frontmatter properties through the live data source schema, creates initial children from directly supported Markdown blocks, and then reconciles the created page into its stable `Exact Row Title/page.md` path, using `Exact Row Title shortid/page.md` only when a sibling name collision requires it;
- unsupported write forms fail before API mutation, including table width or header-mode changes, detected non-trailing table row deletes, page/database creation outside database-row files, computed/read-only properties, local media uploads larger than the 20 MB direct-upload limit, multi-data-source row creation, and rich inline shapes that cannot be represented by the current Markdown parser;
- appends use Notion's current position object, with `start` for prepends and `after_block` for inserts after a known block;
- directive block moves use the same append positioning, then archive the old
  block, because the public Notion API rejects direct existing-block
  repositioning;
- rendered read-only block moves that the planner represents as append+archive,
  such as `link_to_page` lines, preserve the existing Notion payload instead of
  reparsing the Markdown as a paragraph;
- rendered `link_to_page` retargets are blocked before journaled apply because
  the public Notion API does not reliably patch those targets;
- before apply, the connector re-reads the page and compares the current Notion edit timestamp against the Synced Tree version carried by the push executor;
- after apply, the daemon reconciler fetches changed and created pages, rewrites local files atomically, saves refreshed Synced Tree shadows, updates `remote_edited_at`, and removes the temporary source filename when a created row moves into its projected path.

This gives the end-to-end write loop while preserving the rich inline shapes that the renderer emits. The next fidelity step is widening the inline parser to cover nested annotation/link combinations, local relative-file link resolution, and remaining specialized Notion mention variants.

## Schema-Backed Property Validation

Projected database directories carry `_schema.yaml`, generated from the live Notion database data source schema. Before `loc diff` or daemon-backed `loc push` accepts a database row property change, Locality reads that file and validates the frontmatter keys that would actually be written.

For existing rows, only changed frontmatter properties are validated, so read-only values rendered from Notion, such as formulas or rollups, can remain in the file unchanged. For new row files, every non-identity frontmatter property is validated because all of them become create-page payload fields.

The current validator supports the same writable property set as apply: `title`, `rich_text`, `number`, `select`, `status`, `multi_select`, `checkbox`, `date`, `url`, `email`, `phone_number`, external `files`, explicit `people` user IDs, and `relation` page IDs. Select-like values must use option names already present in `_schema.yaml`; unknown options stop as `fix_validation` instead of implicitly creating new Notion options. Computed, read-only, or unresolved types such as `formula`, `rollup`, timestamps, users, `unique_id`, and `verification` are blocked with structured validation errors until their ownership and resolution policies are designed.

Multi-data-source databases still stop before row writes because Locality does not yet have a path-level way to choose the target data source. Pull the database again if `_schema.yaml` is missing or stale.

## Local Media

When Locality writes a Notion page into a local projection, media blocks with `external.url` or Notion-hosted `file.url` render as Markdown image/link syntax. Downloadable image, video, PDF, audio, and generic file links point at an absolute local media file under the projection output root, and those files are downloaded to that root's media tree:

```text
.loc/
  media/
    Roadmap/
      image-0123456789ab.png
      video-abcdef1234567890.mp4
```

The media tree mirrors the Notion page directory under the reserved `.loc/` namespace in the projection output root. This keeps binary files out of content directories while giving agents a stable local file they can open, and avoids collision with a projected Notion page or database named `media`. Locality records downloaded media metadata and checksums in `.loc/media/manifest.json` using mount-relative paths. `loc status`, `loc inspect`, `loc diff`, and `loc push` treat equivalent relative and projection-output-root absolute media hrefs as the same asset. If the resolved local media path, bytes, or caption changes, `loc diff` plans an `update_media` operation and `loc push` uploads the local file to the existing Notion media block. Appending a new Markdown image or link whose href resolves under the projection output root's `.loc/media/` tree uploads that file and creates a Notion image, video, audio, PDF, or generic file block based on the file MIME type. Single-part uploads are capped at 20 MB until multipart upload support exists.

## Path Projection

Root-page mounts use a stable directory shape based on the remote title:
`Exact Page Title/page.md`. Locality preserves spaces, casing, punctuation, and
Unicode when the local filesystem can represent them. Characters that cannot
safely live in a path segment are minimally replaced. When two siblings project
to the same filesystem-equivalent path, all colliding siblings use a short
remote ID suffix such as `Exact Page Title aaaaaa/page.md`. The suffix lengthens
only when needed to keep sibling names unique.

Each Notion page is a directory. The page body lives in `page.md`; sibling entries in the same directory are child Notion content:

```text
Roadmap/
  page.md
  Design Notes/
    page.md
  Tasks/
```

The remote ID remains the identity. The projected title path can change without
changing identity.

Database blocks project as directories. Each data source under the database contributes row pages directly inside that directory, and `_schema.yaml` mirrors the current property schema with stable property IDs, types, and select/status option names.

```text
Roadmap/
  page.md
  Tasks/
    _schema.yaml
    Fix login bug/
      page.md
```

Row directories are normal page directories. Their `page.md` stubs include page identity plus supported property values in frontmatter, while the body remains the standard Locality stub marker until hydration. Creating a row accepts either the canonical page-directory shape (`database/new-row/page.md`) or the ergonomic shortcut (`database/new-row.md`) with YAML frontmatter and no `loc.id`; `loc push -y` creates the Notion page, reads it back, saves the durable entity/shadow rows, and replaces shortcut files with the canonical projected row directory. `_view.csv` remains future work.
