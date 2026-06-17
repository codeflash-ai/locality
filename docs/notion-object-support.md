# Notion Object Support Matrix

This matrix is the working contract for the Notion connector. It tracks public
Notion API objects against the current AFS behavior, not the full Notion UI.

Sources used for the baseline:

- Notion Block object reference: https://developers.notion.com/reference/block
- Notion Create page reference: https://developers.notion.com/reference/post-page
- Notion Page property values reference: https://developers.notion.com/reference/page-property-values
- Notion Data source property schema reference: https://developers.notion.com/reference/property-object

## API Objects

| Notion object | AFS support | Tests | Notes |
|---|---:|---|---|
| Page | Read, render, edit supported blocks, edit supported properties | fixture, live | Page body is block content; page metadata/properties are frontmatter. |
| Block | Recursive read/render; write subset | fixture, live | Unsupported/lossy blocks render as anchored directives and are protected by directive validation. |
| Database | Read/enumerate as directory | fixture, live | Database containers project to directories. |
| Data source | Read/query rows, render `_schema.yaml`, validate row property writes, create rows when database has exactly one data source | fixture, live, mounted live | Multi-data-source row writes are intentionally blocked until path/schema selection exists. |
| User | Read when embedded in mentions/properties; writable by explicit ID in people properties | fixture, live property write | User objects are not mounted as standalone files in v1. |
| Comment | Unsupported | none | Comments are not in the v1 filesystem model from `plan.md`; adding them needs a thread representation and write policy. |
| File upload | Supported for existing image block uploads from local `.afs/media/`; external/download URLs are read and external file properties are writable | fixture, live image download/upload, live property write | Direct local uploads are currently limited to image blocks and small single-part uploads. Non-image uploads still need retention, size, dedupe, and local path ownership policy. |
| View | Unsupported | none | Views are database presentation state, not row/page content. |
| Custom emoji | Unsupported | none | Emoji metadata is presentation state; emoji text still appears through rich text/plain text. |
| Webhook event | Unsupported locally | none | Webhooks belong to the optional relay path, not the local direct connector. |

## Blocks

| Block type | Read/render | Write | Tests | Notes |
|---|---:|---:|---|---|
| `paragraph` | Native Markdown | Yes | fixture, live | Rich text is rendered inline. |
| `heading_1` | Native Markdown | Yes | fixture, live | `#` heading. |
| `heading_2` | Native Markdown | Yes | fixture, live | `##` heading. |
| `heading_3` | Native Markdown | Yes | fixture, live | `###` heading. |
| `heading_4` | Native Markdown | Yes | fixture, live | `####` heading. |
| `bulleted_list_item` | Native Markdown | Yes | fixture, live | One Notion block per list item. |
| `numbered_list_item` | Native Markdown | Yes | fixture, live | One Notion block per list item. |
| `to_do` | Native Markdown checkbox | Yes | fixture, live | Checked state round-trips through `- [ ]` / `- [x]`. |
| `quote` | Native Markdown quote | Yes | fixture, live | `>` quote. |
| `callout` | Native Markdown callout | Yes | fixture, live read | `> [!NOTE]` callouts update and append as Notion callout blocks. |
| `code` | Native fenced code | Yes | fixture, live | Language is preserved on simple code fences. |
| `divider` | Native Markdown rule | Yes | fixture, live | `---`. |
| `equation` | Native display math | Yes | fixture, live | `$$ ... $$`. |
| `table` | Native Markdown table | Yes for existing tables with stable width/header mode | fixture, live read/write | Existing cell edits update table rows. Added Markdown rows append Notion `table_row` children; removed trailing rows archive row blocks. Width and header-mode changes are still blocked. |
| `table_row` | Structural inside tables | No | fixture | Standalone/malformed rows render as directives. |
| `child_page` | Markdown link and structural enumeration | No direct block write | fixture, live read | Parent pages show a readable link to the child page's stable Notion URL; edit the child through its projected Markdown file. New child pages are created through page/entity creation, not block edits. |
| `child_database` | Directive and structural enumeration | No direct block write | fixture, live read | Databases are created through the database API, not Markdown block writes. |
| `toggle` | Directive wrapper; children render below it | No | fixture, live read | Toggle wrapper state is anchored to avoid flattening nested content. |
| `embed` | Markdown link | Yes for existing blocks | fixture, live read/write | Caption becomes link text; URL edits update the existing embed block. |
| `bookmark` | Markdown link | Yes for existing blocks | fixture, live read/write | Caption becomes link text; URL edits update the existing bookmark block. |
| `link_preview` | Markdown link | Read only | fixture | Renders as a normal link when the API returns a URL; the current create-page API rejected it as a child block in live testing, so writes stay blocked. |
| `image` | Markdown image with local `.afs/media/` href plus local image download | Yes for existing URL blocks and local image uploads | fixture, live read/write/download/upload | Uses `external.url` or Notion-hosted `file.url` as the source of the downloaded local file. Remote URL Markdown edits write external URLs; local `.afs/media/` href edits upload the local image file back to the existing block. URL-less payloads fall back to directives. |
| `video` | Markdown link | Yes for existing URL blocks | fixture, live read/write | Uses `external.url` or Notion-hosted `file.url`; Markdown edits write external URLs. Local download intentionally skipped for now. |
| `file` | Markdown link | Yes for existing URL blocks | fixture, live read/write | Uses `external.url` or Notion-hosted `file.url`; Markdown edits write external URLs. Local download intentionally skipped for now. |
| `pdf` | Markdown link | Yes for existing URL blocks | fixture, live read/write | Uses `external.url` or Notion-hosted `file.url`; Markdown edits write external URLs. Local download intentionally skipped for now. |
| `audio` | Markdown link | Yes for existing URL blocks | fixture, live read/write | Uses `external.url` or Notion-hosted `file.url`; Markdown edits write external URLs. Local download intentionally skipped for now. |
| `synced_block` | Directive wrapper; source block ID preserved when present | No | fixture | Rewriting synced blocks is lossy without source/copy semantics; live creation of an original synced block was rejected because Notion requires `synced_from`. |
| `link_to_page` | Markdown link to Notion URL | Read/delete/move only | fixture, live read, blocked-write regression | Page/database target ID is preserved in the link target; direct retargeting is blocked because Notion ignores direct target PATCHes and replacement needs undo-aware block identity support. |
| `table_of_contents` | Directive | No | fixture, live read | Generated navigation block; no useful Markdown edit surface. |
| `breadcrumb` | Directive | No | fixture, live read | Generated navigation block; no useful Markdown edit surface. |
| `column_list` | Directive wrapper; children render below it | No | fixture, live read | Layout is anchored; child content remains readable. |
| `column` | Directive wrapper; children render below it | No | fixture, live read | Layout is anchored; child content remains readable. |
| `template` | Directive | No | fixture | Deprecated/automation-like block; writing is intentionally blocked. |
| `meeting_notes` | Directive | No | fixture | Not generally createable/editable as normal API page content. |
| `transcription` | Directive | No | fixture | Not generally createable/editable as normal API page content. |
| `tab` | Directive | No | fixture | Newer layout/navigation block; no safe writer yet. |
| `ai_block` | Directive | No | fixture | AI-generated/native Notion object; no safe writer. |
| `custom_block` | Directive | No | fixture | Unknown/custom native payload; no safe writer. |
| `button` | Directive | No | fixture | Button actions are not Markdown content. |
| Unknown future block | Directive | No | fixture | Forward compatibility path: preserve block ID and avoid lossy edits. |

## Rich Text

| Rich text object | Read/render | Write | Tests | Notes |
|---|---:|---:|---|---|
| Text | Yes | Yes | fixture, live | Plain text is escaped conservatively. |
| External text link | Markdown link | Yes | fixture, live | Link URL is preserved. |
| Equation span | Inline math | Yes | fixture, live | `$...$`. |
| Bold, italic, strikethrough, underline, code | Markdown/HTML inline formatting | Yes for emitted shapes | fixture, live | Underline uses `<u>`. |
| Page mention | Markdown link to Notion URL; explicit `@page(...)` write syntax | Yes through Notion-hosted URL, explicit ID syntax, or legacy `afs://` parsing path | fixture, live read/write | Stable ID is preserved; external UUID-shaped links remain ordinary links. Agents can write `@page(11111111-1111-1111-1111-111111111111)` or `@page(Name <11111111-1111-1111-1111-111111111111>)`. |
| Database mention | Markdown link to Notion URL; explicit `@database(...)` write syntax | Yes through explicit ID syntax; label edits preserve database type when target ID is unchanged | fixture, live read/write | Stable ID is preserved. Agents can write `@database(11111111-1111-1111-1111-111111111111)` or `@database(Name <11111111-1111-1111-1111-111111111111>)`. |
| User mention | Plain `@name`/fallback; explicit `@user(...)` write syntax | Yes through explicit ID syntax | fixture, live read/write | Agents can write `@user(11111111-1111-1111-1111-111111111111)` or `@user(Name <11111111-1111-1111-1111-111111111111>)`; name/email lookup is deferred. |
| Date mention | Plain date/range text; explicit `@date(...)` write syntax | Yes through explicit syntax | fixture, live read/write | Agents can write `@date(2026-06-14)` or `@date(2026-06-14 to 2026-06-21, tz=America/Chicago)` when the result must remain a typed Notion date mention. Plain dates stay plain text unless preserved from the preimage. |
| Link preview mention | Markdown link | Read only | fixture, live API probe | Preserves URL on read. Current Notion write validation rejects `mention.link_preview` in page child rich text payloads, so AFS must not synthesize or preserve it through edited writes yet. |
| Unknown mention variants | Plain text fallback | No | fixture | Avoids losing visible content while blocking typed edits. |

## Page And Data Source Properties

| Property type | Read/frontmatter | Write | Tests | Notes |
|---|---:|---:|---|---|
| `title` | Yes | Yes | fixture, live, mounted live, schema | Title is the canonical `title` frontmatter field. |
| `rich_text` | Yes with inline Markdown | Yes with the body rich-text Markdown parser | fixture, live, mounted live, schema | Frontmatter preserves supported annotations, external links, equations, and explicit typed mention syntax instead of flattening to plain text. |
| `number` | Yes | Yes | fixture, live, mounted live, schema | Numeric validation happens before API call. |
| `select` | Yes | Yes | fixture, live, mounted live, schema | Option names must exist in `_schema.yaml`. |
| `status` | Yes | Yes | fixture, live, mounted live, schema | Option names must exist in `_schema.yaml`. |
| `multi_select` | Yes | Yes | fixture, live, mounted live, schema | List values must exist in `_schema.yaml`. |
| `checkbox` | Yes | Yes | fixture, live, mounted live, schema | Boolean. |
| `date` | Yes | Yes | fixture, live, mounted live, schema | String date or map with `start`/`end`/`time_zone`. |
| `url` | Yes | Yes | fixture, live, mounted live, schema | Nullable HTTP/HTTPS string. |
| `email` | Yes | Yes | fixture, live, mounted live, schema | Nullable email string. |
| `phone_number` | Yes | Yes | fixture, live, mounted live, schema | Nullable string. |
| `files` | Yes | Yes for external URLs | fixture, live read/write, schema | Frontmatter accepts `https://...` or `Name <https://...>` entries and writes Notion external file objects. Hosted/uploaded file ownership remains read-only. |
| `people` | Yes | Yes for explicit user IDs | fixture, live read/write, schema | Frontmatter accepts a Notion user ID string, `Name <user-id>`, or a list. User lookup by name/email is deferred. |
| `relation` | Yes | Yes for explicit page IDs | fixture, live read/write, schema | Frontmatter accepts a Notion page ID string or list of page IDs. Path/title resolution is deferred. |
| `formula` | Yes | No | fixture, schema-blocked | Computed/read-only by Notion. |
| `rollup` | Yes | No | fixture, schema-blocked | Computed/read-only by Notion. |
| `created_time` | Yes | No | fixture | Read-only by Notion. |
| `created_by` | Yes | No | fixture | Read-only by Notion. |
| `last_edited_time` | Yes | No | fixture | Read-only by Notion. |
| `last_edited_by` | Yes | No | fixture | Read-only by Notion. |
| `unique_id` | Yes | No | fixture, live read | Generated by Notion. |
| `verification` | Yes | No | fixture | Wiki/workflow metadata; not a normal row edit field. |
| `button` property | No | No | doc only | Action trigger, not persisted row content for AFS. |

## Current Intentional Gaps

- Non-image downloads/uploads and broader hosted file rewrites are deferred until
  AFS has complete size limits, retention rules, and local path ownership semantics.
- Table width changes and header-mode changes are deferred until the planner can
  represent them without losing Notion table semantics.
- Layout and generated blocks (`column_*`, `breadcrumb`, `table_of_contents`,
  tabs) stay as directives because Markdown cannot represent their semantics.
- Comments are not mounted because they need a separate thread model and push
  policy.
- People writes currently require explicit user IDs; name/email resolution is
  deferred. Relation writes currently require explicit related page IDs;
  path/title resolution is deferred.

## Next Block Work

1. Add fixture-backed write tests before widening any block type. The Tier 1
   writer suite now covers headings, numbered lists, to-dos, quotes, callouts,
   code fences, dividers, and equations.
2. Extend table writes beyond stable-width row edits. Width changes and header
   mode changes need a safer representation than whole-table replacement.
3. Keep layout, generated, synced, and unknown future blocks directive-backed
   until their Notion semantics can be represented without content loss.
4. Broaden media writes beyond existing image blocks only after size limits,
   retention rules, dedupe, and local file ownership decisions are settled.
