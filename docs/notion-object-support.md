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
| Data source | Read/query rows, render `_schema.yaml`, validate row property writes, create rows when database has exactly one data source | fixture, live | Multi-data-source row writes are intentionally blocked until path/schema selection exists. |
| User | Read only when embedded in mentions/properties | fixture | User objects are not mounted as standalone files in v1. |
| Comment | Unsupported | none | Comments are not in the v1 filesystem model from `plan.md`; adding them needs a thread representation and write policy. |
| File upload | Unsupported for upload; external/download URLs are read | fixture, live image download | Uploading files needs retention, size, dedupe, and local path ownership policy. |
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
| `table` | Native Markdown table | No | fixture, live read | Row-level write planning is future work. |
| `table_row` | Structural inside tables | No | fixture | Standalone/malformed rows render as directives. |
| `child_page` | Directive and structural enumeration | No direct block write | fixture, live read | New child pages are created through page/entity creation, not block edits. |
| `child_database` | Directive and structural enumeration | No direct block write | fixture, live read | Databases are created through the database API, not Markdown block writes. |
| `toggle` | Directive wrapper; children render below it | No | fixture, live read | Toggle wrapper state is anchored to avoid flattening nested content. |
| `embed` | Directive | No | fixture, live read | URL preserved. |
| `bookmark` | Directive | No | fixture, live read | URL preserved. |
| `link_preview` | Directive | No | fixture | URL preserved when returned by the API; the current create-page API rejected it as a child block in live testing. |
| `image` | Markdown image plus local image download | No | fixture, live read/download | Uses `external.url` or Notion-hosted `file.url`; URL-less payloads fall back to directives. |
| `video` | Markdown link | No | fixture, live read | Uses `external.url` or Notion-hosted `file.url`; local download intentionally skipped for now. |
| `file` | Markdown link | No | fixture, live read | Uses `external.url` or Notion-hosted `file.url`; local download intentionally skipped for now. |
| `pdf` | Markdown link | No | fixture, live read | Uses `external.url` or Notion-hosted `file.url`; local download intentionally skipped for now. |
| `audio` | Markdown link | No | fixture, live read | Uses `external.url` or Notion-hosted `file.url`; local download intentionally skipped for now. |
| `synced_block` | Directive wrapper; source block ID preserved when present | No | fixture | Rewriting synced blocks is lossy without source/copy semantics; live creation of an original synced block was rejected because Notion requires `synced_from`. |
| `link_to_page` | Directive | No | fixture, live read | Page/database target ID preserved. |
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
| Page mention | `afs://` link | Read; write via supported `afs://` parsing path | fixture | Stable ID is preserved. |
| Database mention | `afs://` link | Read only in current live suite | fixture | Stable ID is preserved. |
| User mention | Plain `@name`/fallback | Read only | fixture | Needs identity lookup before safe writes. |
| Date mention | Plain date/range text | Read only | fixture, live | Needs typed date mention parser before safe writes. |
| Link preview mention | Markdown link | Read only | fixture | Preserves URL. |
| Unknown mention variants | Plain text fallback | No | fixture | Avoids losing visible content while blocking typed edits. |

## Page And Data Source Properties

| Property type | Read/frontmatter | Write | Tests | Notes |
|---|---:|---:|---|---|
| `title` | Yes | Yes | fixture, live, schema | Title is the canonical `title` frontmatter field. |
| `rich_text` | Yes | Yes | fixture, live, schema | Written as plain rich text today. |
| `number` | Yes | Yes | fixture, live, schema | Numeric validation happens before API call. |
| `select` | Yes | Yes | fixture, live, schema | Option names must exist in `_schema.yaml`. |
| `status` | Yes | Yes | fixture, live, schema | Option names must exist in `_schema.yaml`. |
| `multi_select` | Yes | Yes | fixture, live, schema | List values must exist in `_schema.yaml`. |
| `checkbox` | Yes | Yes | fixture, live, schema | Boolean. |
| `date` | Yes | Yes | fixture, live, schema | String date or map with `start`/`end`/`time_zone`. |
| `url` | Yes | Yes | fixture, live, schema | Nullable HTTP/HTTPS string. |
| `email` | Yes | Yes | fixture, live, schema | Nullable email string. |
| `phone_number` | Yes | Yes | fixture, live, schema | Nullable string. |
| `files` | Yes | No | fixture, live read-empty, schema-blocked | File upload/link ownership policy is not designed yet. |
| `people` | Yes | No | fixture, live read-empty, schema-blocked | Needs user lookup and permission-aware validation before writes. |
| `relation` | Yes | No | fixture, schema-blocked | Needs target data-source schema and path/ID resolution before writes. |
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

- Media upload and non-image downloads are deferred until AFS has size limits,
  retention rules, and local path ownership semantics.
- Table writes are deferred until the planner can produce row-level operations
  instead of replacing the whole table.
- Layout and generated blocks (`column_*`, `breadcrumb`, `table_of_contents`,
  tabs) stay as directives because Markdown cannot represent their semantics.
- Comments are not mounted because they need a separate thread model and push
  policy.
- People/relation writes are blocked until validation can resolve user IDs and
  related page IDs from local references.

## Next Block Work

1. Add fixture-backed write tests before widening any block type. The Tier 1
   writer suite now covers headings, numbered lists, to-dos, quotes, callouts,
   code fences, dividers, and equations.
2. Treat tables as the next large design item. They need row-level diff/apply
   rather than whole-table replacement.
3. Keep layout, generated, synced, and unknown future blocks directive-backed
   until their Notion semantics can be represented without content loss.
4. Design media writes separately from text block writes. Upload support needs
   size limits, retention rules, dedupe, and local file ownership decisions.
