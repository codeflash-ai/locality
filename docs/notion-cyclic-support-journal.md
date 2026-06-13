# Notion Cyclic Support Journal

This journal records Notion support added while expanding live cyclic tests. It
is separate from the support matrix so reviewers can see why a behavior changed
and what Markdown shape agents should expect.

## 2026-06-13

### Page And Database Links

- **Notion input:** `link_to_page` blocks with `page_id` or `database_id`.
- **Markdown output:** Valid targets render as normal links:
  - `[Linked page](https://www.notion.so/<page-id>)`
  - `[Linked database](https://www.notion.so/<database-id>)`
- **Write behavior:** Unchanged link blocks are preserved during pushes. Direct
  retargeting of a `link_to_page` block is not yet supported; malformed native
  link payloads still render as guarded AFS directives.
- **Inline mentions:** Page and database rich-text mentions now render as normal
  Notion URL links instead of `afs://` links. The writer accepts page URLs on
  Notion hosts as page mention writes and keeps legacy `afs://` parsing for
  compatibility. External links with UUID-shaped paths remain ordinary links.

### Mounted Live Cyclic Coverage

- **Read/no-op cycle:** The live test creates a page containing paragraphs,
  rich text annotations, inline page mentions, headings 1-4, lists, to-dos,
  quote, callout, toggle children, code, divider, equation, bookmark, embed,
  table, column layout, table of contents, breadcrumb, link-to-page, child page,
  and external media blocks. It mounts and pulls the page, validates the Markdown
  projection, performs a no-op push, and verifies Notion block JSON is unchanged.
- **Edit/push cycle:** The live test creates a supported-edit page, edits each
  supported Markdown block shape locally, pushes, and verifies the rendered
  Notion content through the Notion API.

### Mounted Database Row Cycles

- **Projection:** A live child database is mounted as a directory with
  `_schema.yaml`; existing rows appear as Markdown files under that directory.
- **Read/no-op cycle:** The live test creates a database row with title,
  rich-text, number, select, status, multi-select, checkbox, date, URL, email,
  and phone properties. It hydrates the row file through the mount, performs a
  no-op push, and verifies the Notion page bundle is unchanged.
- **Edit/push cycle:** The test edits row frontmatter and body from the mounted
  Markdown file, pushes, and verifies the expected frontmatter/body render from
  a fresh Notion API fetch.
- **Create cycle:** The test writes a new Markdown file under the database
  directory, pushes it as a new Notion row, and verifies the created row's
  properties and body through the Notion API.

### Bookmark And Embed URL Blocks

- **Notion input:** `bookmark` and `embed` blocks with URL and optional caption.
- **Markdown output:** Valid blocks render as normal Markdown links:
  - `[Bookmark caption](https://example.com/bookmark)`
  - `[Embed caption](https://example.com/embed)`
- **Write behavior:** Existing bookmark/embed blocks can be edited by changing
  the Markdown link label or URL. A malformed URL block with no URL still falls
  back to an AFS directive instead of becoming lossy Markdown.
- **Verification:** Fixture apply tests assert the exact Notion update payloads,
  and the live mounted edit cycle updates bookmark/embed links then verifies the
  rendered Notion result through the API.
