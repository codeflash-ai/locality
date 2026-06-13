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

### External Media URL Blocks

- **Notion input:** `image`, `video`, `file`, `pdf`, and `audio` blocks with
  `external.url` or Notion-hosted `file.url` plus optional captions.
- **Markdown output:** Images render as Markdown image syntax; other media
  blocks render as Markdown links:
  - `![Image caption](https://example.com/image.png)`
  - `[File caption](https://example.com/file.pdf)`
- **Write behavior:** Existing media blocks can be edited by changing the
  Markdown label or URL. Writes use Notion external media URLs; local uploads,
  new media block appends, and local file attachment ownership remain deferred.
- **Verification:** Fixture apply tests assert exact update payloads for every
  media kind. The live mounted edit cycle updates media captions, pushes them,
  and verifies the rendered Notion result through the API.
- **Bug fixed during live testing:** The initial writer reused the create-block
  media payload shape and sent `type: external` during block updates. The live
  Notion update endpoint rejected that field, so the update payload now sends
  only the nested `external.url` and `caption` fields for media block updates.

### Link Preview Blocks

- **Notion input:** `link_preview` blocks with a returned URL and optional
  caption/title text.
- **Markdown output:** Link previews render as normal Markdown links, matching
  bookmark/embed readability without exposing an AFS directive for URL-shaped
  content:
  - `[Preview](https://example.com/preview)`
- **Write behavior:** Link preview writes remain blocked. Live create-page
  testing rejected `link_preview` as a child block, so AFS does not yet have a
  safe write or append contract for this block type.
- **Verification:** Fixture render coverage asserts that a returned
  `link_preview` block renders to Markdown link syntax.

### Same-Shape Table Cell Edits

- **Notion input:** Simple `table` blocks with `table_row` children and no
  nested row children.
- **Markdown output:** Tables render as standard Markdown tables. Existing
  column-header tables map the first Markdown row to the first Notion table row;
  headerless tables keep the renderer's empty Markdown header marker and map
  data lines to Notion rows.
- **Write behavior:** A Markdown edit to an existing table updates the
  corresponding Notion `table_row.cells` values when table width, row count, and
  header flags are unchanged. Row additions, row deletions, width changes, and
  header-mode changes fail before API mutation.
- **Verification:** Core diff coverage asserts that table edits produce a table
  block update rather than archive/recreate. Fixture apply tests assert exact
  row update payloads, and the live mounted edit cycle updates a table cell then
  verifies the rendered Notion result through the API.
- **Bug fixed during live testing:** The live database fixtures used a fixed
  Notion unique-ID prefix, which can collide at workspace scope on repeated
  runs. The live fixtures now generate a short unique alphanumeric prefix for
  each scratch database.

### External File Properties

- **Notion input:** Database/page `files` properties containing external files
  or Notion-hosted files.
- **Markdown output:** File properties render as frontmatter lists. Entries with
  both name and URL use `Name <https://example.com/file.pdf>`; URL-only entries
  render as the URL string.
- **Write behavior:** Frontmatter edits can write external file URLs using
  either `https://example.com/file.pdf` or `Name <https://example.com/file.pdf>`
  list entries. Uploading local files, rewriting hosted Notion files, and
  retention/dedupe policy remain deferred.
- **Verification:** Fixture apply tests assert exact page update and row create
  payloads. Schema tests validate accepted/rejected file frontmatter. The live
  mounted database cycle edits and creates rows with file properties, and the
  live direct integrity test creates and updates a file property through the API.

### Relation Properties

- **Notion input:** Database/page `relation` properties containing related page
  IDs. Live fixtures create the target database first, then create a
  single-property relation schema pointing at that target data source.
- **Markdown output:** Relation properties render as frontmatter lists of
  Notion page IDs, matching the current read projection.
- **Write behavior:** Frontmatter edits can write a string or YAML list of
  explicit Notion page IDs. Clearing with `null`, an empty string, or an empty
  list is supported by the same writer shape. Resolving relation targets by
  local path, row title, or workspace search remains deferred.
- **Live finding:** Current Notion relation schema creation rejects a relation
  with only `data_source_id`; the live fixture must include
  `single_property: {}` or `dual_property` in the relation schema.
- **Verification:** Fixture apply tests assert exact page update and row create
  payloads. Schema tests validate accepted/rejected relation frontmatter. The
  live mounted database cycle reads, creates, and verifies relation properties,
  and the live direct integrity test creates then updates a relation property
  through the API.

### People Properties

- **Notion input:** Database/page `people` properties containing user objects.
  The live PAT test uses the token's bot user ID from `/v1/users/me`.
- **Markdown output:** People properties render as frontmatter lists. Entries
  with both display name and ID use `Name <user-id>`; ID-only entries render as
  the ID string.
- **Write behavior:** Frontmatter edits can write a string or YAML list of
  explicit Notion user IDs. `Name <user-id>` is accepted so the rendered shape
  can round-trip. Clearing with `null`, an empty string, or an empty list is
  supported by the same writer shape. Name/email lookup remains deferred.
- **Verification:** Fixture apply tests assert exact page update and row create
  payloads. Schema tests validate accepted/rejected people frontmatter. The
  live mounted database cycle starts with an empty people property, writes the
  bot user through a mounted Markdown edit, and verifies the rendered Notion
  result. The live direct integrity test creates a people value and then clears
  it through the API writer.

### Database Mentions As Markdown Links

- **Notion input:** Rich-text database mentions and `link_to_page` blocks that
  target databases.
- **Markdown output:** Both render as normal Markdown links to Notion URLs,
  matching page mention/link behavior.
- **Write behavior:** When a rendered database mention link is edited only in
  label text and the Notion target ID is unchanged, the rich-text parser writes
  it back as a typed Notion database mention instead of accidentally converting
  it to a page mention. Creating arbitrary new database mentions from a plain
  Notion URL still needs an explicit typed-link syntax because Notion page and
  database URLs are not distinguishable from the ID alone.
- **Verification:** Fixture apply tests assert edited database mention links
  produce `mention.database` payloads. The live diverse page cyclic test creates
  both a rich-text database mention and a database `link_to_page` block, then
  verifies the mounted read/no-op push does not mutate the Notion block JSON.
