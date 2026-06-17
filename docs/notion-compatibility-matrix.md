# Notion Compatibility Matrix

This matrix describes what the Notion connector currently exposes through the
AFS filesystem projection. It is written for users and agents editing mounted
Notion content. For lower-level API object coverage, see
`docs/notion-object-support.md`.

Support terms:

- **Read/write** means AFS renders the Notion feature to local files and can
  push supported local edits back to Notion.
- **Read-only** means AFS renders the feature but blocks edits that would be
  lossy or unsafe.
- **Unsupported** means AFS does not currently expose a safe filesystem edit
  surface for the feature.

## Workspace And Filesystem Projection

| Notion feature | Support | Filesystem shape | Notes |
|---|---|---|---|
| Workspace page tree | Read/enumerate | Directories and Markdown files | Top-level pages and nested pages/databases can be discovered through the mounted tree. |
| Page body | Read/write for supported blocks | Markdown file | Page content is rendered as Markdown with frontmatter for page metadata when available. |
| Child page | Read/enumerate | Parent body link plus nested Markdown file | Parent pages show a readable link with the stable Notion page URL. The child page content is edited through the projected child Markdown file. New child pages are created through entity creation, not arbitrary block edits. |
| Database / data source | Read/enumerate | Directory | Database rows appear as Markdown files under the database directory. |
| Data source schema | Read/validation source | `_schema.yaml` | Used to validate database row property edits before writing to Notion. |
| Database row | Read/write/create row | Markdown file with frontmatter | Editing row body and supported properties is supported. Creating a new Markdown file under a database directory creates a row when the database has one writable data source. |
| Image asset cache | Read/download for images | `media/` directory | Images referenced by supported image blocks are copied locally so agents can inspect them. |

## Blocks

| Notion feature | Support | Markdown shape | Notes |
|---|---|---|---|
| Paragraph | Read/write | Paragraph text | Rich text is rendered inline. |
| Heading 1 | Read/write | `# Heading` |  |
| Heading 2 | Read/write | `## Heading` |  |
| Heading 3 | Read/write | `### Heading` |  |
| Heading 4 | Read/write | `#### Heading` |  |
| Bulleted list item | Read/write | `- item` | One Notion block per Markdown list item. |
| Numbered list item | Read/write | `1. item` | One Notion block per Markdown numbered item. |
| To-do | Read/write | `- [ ] task` / `- [x] task` | Checkbox state round-trips through Markdown task syntax. |
| Quote | Read/write | `> quote` |  |
| Callout | Read/write | `> [!NOTE] ...` | Text content is writable. Emoji/color presentation is not the main edit surface yet. |
| Code | Read/write | Fenced code block | Simple language and content edits round-trip. |
| Divider | Read/write | `---` |  |
| Equation | Read/write | Display math block | Inline equations are covered by rich text. |
| Table | Read/write with stable shape | Markdown table | Cell edits, row appends, and trailing row deletes are supported. Width/header-mode changes are blocked. |
| Bookmark | Read/write for existing blocks | Markdown link | Caption and URL edits update the existing block. |
| Embed | Read/write for existing blocks | Markdown link | Caption and URL edits update the existing block. |
| Link preview | Read-only | Markdown link | The current Notion API rejects safe creation/write shapes for this block. |
| Child page link | Read; direct edit blocked | Markdown link to Notion page | The link target carries the stable page ID for lookup. Edit the child page's `page.md` or title frontmatter rather than the parent link. |
| Link to page | Read; retarget blocked | Markdown link to Notion page | Direct target PATCH is not reliable in the Notion API, so retargeting is guarded. |
| Link to database | Read; retarget blocked | Markdown link to Notion database | Replacement needs undo-aware block identity support before AFS can write it safely. |
| Image with external or Notion-hosted URL | Read/write for existing URL blocks | Markdown image | Existing URL/caption edits push. Local uploads are not supported yet. |
| Video with external or Notion-hosted URL | Read/write for existing URL blocks | Markdown link | Existing URL/caption edits push. |
| File with external or Notion-hosted URL | Read/write for existing URL blocks | Markdown link | Existing URL/caption edits push. |
| PDF with external or Notion-hosted URL | Read/write for existing URL blocks | Markdown link | Existing URL/caption edits push. |
| Audio with external or Notion-hosted URL | Read/write for existing URL blocks | Markdown link | Existing URL/caption edits push. |
| Toggle | Read-only wrapper | Guarded directive with readable children | Child content remains visible; wrapper semantics are protected. |
| Column list | Read-only wrapper | Guarded directive with readable children | Layout semantics are not editable as Markdown yet. |
| Column | Read-only wrapper | Guarded directive with readable children | Layout semantics are not editable as Markdown yet. |
| Table of contents | Read-only | Guarded directive | Generated navigation block with no meaningful Markdown edit surface. |
| Breadcrumb | Read-only | Guarded directive | Generated navigation block with no meaningful Markdown edit surface. |
| Synced block | Read-only | Guarded directive | Source/copy semantics are protected to avoid lossy writes. |
| Template, meeting notes, transcription, tab, AI block, custom block, button | Read-only or unsupported | Guarded directive when returned by the API | These blocks do not yet have a safe Markdown writer. |
| Unknown future block | Read-only | Guarded directive | AFS preserves the block ID and blocks lossy edits. |

## Rich Text

| Notion feature | Support | Markdown shape | Notes |
|---|---|---|---|
| Plain text | Read/write | Text | Escaped conservatively when needed. |
| Bold, italic, strikethrough, underline, inline code | Read/write | Markdown/HTML inline formatting | Underline uses HTML because Markdown has no native underline. |
| External link | Read/write | Markdown link | URL and label edits push. |
| Inline equation | Read/write | Inline math |  |
| Page mention | Read/write by ID | Notion URL link or `@page(...)` | Existing mentions preserve their typed identity. New typed mentions should use explicit IDs. |
| Database mention | Read/write by ID | Notion URL link or `@database(...)` | Explicit database syntax avoids ambiguity with page URLs. |
| Date mention | Read/write with explicit syntax | Readable date text or `@date(...)` | Plain date-looking text is not auto-promoted to a typed Notion mention. |
| User mention | Read/write by ID | Readable mention text or `@user(...)` | Name/email lookup is deferred; write explicit Notion user IDs. |
| Link-preview mention | Read-only | Markdown link | Notion write validation currently rejects safe synthesis of this mention type. |
| Unknown mention variant | Read-only fallback | Plain text or directive | AFS avoids silently flattening unsupported typed objects. |

## Database Properties

| Property type | Support | Filesystem shape | Notes |
|---|---|---|---|
| Title | Read/write | `title` frontmatter | The canonical row/page title. |
| Rich text | Read/write | Frontmatter string with inline Markdown | Preserves supported annotations, links, equations, and explicit typed mention syntax. |
| Number | Read/write | Frontmatter number | Validated before API calls. |
| Select | Read/write | Frontmatter string | Option must exist in `_schema.yaml`. |
| Status | Read/write | Frontmatter string | Option must exist in `_schema.yaml`. |
| Multi-select | Read/write | Frontmatter list | Options must exist in `_schema.yaml`. |
| Checkbox | Read/write | Frontmatter boolean |  |
| Date | Read/write | Frontmatter string or structured range | Supports scalar dates and `start`/`end`/`time_zone` shapes. |
| URL | Read/write | Frontmatter string or null |  |
| Email | Read/write | Frontmatter string or null |  |
| Phone number | Read/write | Frontmatter string or null |  |
| Files | Read/write for external URLs | Frontmatter list | Accepts raw HTTPS URLs or `Name <https://...>` entries. Hosted/uploaded file ownership remains read-only. |
| People | Read/write by explicit user ID | Frontmatter string or list | Accepts Notion user IDs or `Name <user-id>`. Name/email lookup is deferred. |
| Relation | Read/write by explicit page ID | Frontmatter string or list | Accepts related page IDs. Path/title lookup is deferred. |
| Formula | Read-only | Frontmatter value | Computed by Notion. |
| Rollup | Read-only | Frontmatter value | Computed by Notion. |
| Created time, created by, last edited time, last edited by | Read-only | Frontmatter value | Managed by Notion. |
| Unique ID | Read-only | Frontmatter value | Generated by Notion. |
| Verification | Read-only | Frontmatter value when returned | Not a normal row edit field. |
| Button property | Unsupported write | Not exposed as editable content | Action trigger, not persisted row content for AFS. |

## Intentional Gaps

- Local media upload and hosted-file rewrites are deferred until AFS has size
  limits, retention rules, dedupe, and local path ownership semantics.
- Table width changes and header-mode changes are blocked until the planner can
  represent them without replacing the table unsafely.
- Layout and generated navigation blocks stay directive-backed because Markdown
  cannot represent their semantics.
- Comments are not mounted yet; they need a separate thread model and write
  policy.
- People and relation writes currently require explicit Notion IDs.
