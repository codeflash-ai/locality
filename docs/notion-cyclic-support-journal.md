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
