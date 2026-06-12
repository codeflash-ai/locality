# Notion Canonical Format

Notion pages render as Markdown with YAML frontmatter.

```markdown
---
afs:
  id: a3f2c8d1-...
  type: page
  parent: 9c1b...
  synced_at: 2026-06-09T14:02:11Z
  remote_edited_at: 2026-06-09T13:58:40Z
title: Roadmap 2026
status: In progress
owner: saurabh@example.com
---
# Roadmap 2026

Q2 priorities are...
```

Clean Markdown is preferred for diffable blocks. Undiffable or lossy blocks render as single-line directives, for example:

```text
::afs{id=b771 type=synced_block title="Shared header"}
```

Directive integrity is validated before push. Agents may move directive lines as whole lines, but editing directive contents is rejected unless the change maps to an explicit supported operation.

The first renderer supports common text blocks, richer inline text, display equations, simple tables, and file-like media blocks with API URLs directly. Inline bold, italic, strikethrough, code, external links, date mentions, page/database mentions, link previews, and equations use ordinary Markdown or small HTML fallbacks when Markdown has no native equivalent. Child pages, child databases, toggles, embeds, bookmarks, synced blocks, column layouts, tabs, meeting notes, AI/custom blocks, URL-less media payloads, and unsupported/lossy blocks render as directives. This keeps the page inspectable while preserving remote block IDs for later safer round-trip support.

Media blocks with a Notion `file.url` or `external.url` render as ordinary Markdown. Images use image syntax, while other file-like blocks use links:

```markdown
![Architecture diagram](https://...)
[Design brief](https://...)
```

When rendered through a filesystem-aware pull or reconcile path, image files are also downloaded into the mount-level `media/` directory so agents can open a local copy without cluttering the Markdown page directory. URL-less media payloads still render as directives, for example `::afs{id=image-id type=image title="Architecture diagram"}`.

The first writer supports block bodies whose Markdown shape maps to one Notion block: paragraphs, headings, single list items, to-dos, quotes, code fences, dividers, and display equations. It also parses the rich inline Markdown emitted by the renderer for bold, italic, strikethrough, underline, code, external links, equations, and `afs://` page links. Unchanged preimage mentions, such as date mentions, are preserved during block updates; unsupported inline shapes fail rather than being flattened silently.

## Database Rows

A Notion database row renders as the same page document shape with row properties flattened into frontmatter keys:

```markdown
---
afs:
  id: b771...
  type: page
  synced_at: "2026-06-10T23:26:00Z"
  remote_edited_at: "2026-06-10T23:26:00Z"
title: "Fix login bug"
"Status": "In progress"
"Points": 3
"Tags":
  - "Backend"
  - "Docs"
---
```

Supported read-side property values include title, rich text, number, select, multi-select, status, checkbox, date, URL, email, phone, files, people, relation IDs, created/edited timestamps, created/edited users, formula, rollup, unique ID, and verification values.

Property writes are planned by comparing edited frontmatter against the shadow frontmatter captured during the last render. The Notion writer currently applies title, rich text, number, select, status, multi-select, checkbox, date, URL, email, and phone properties. Read-only, computed, or identity-backed property classes such as files, people, relation, formula, rollup, created/edited timestamps, created/edited users, unique ID, and verification remain read-side only until schema validation and richer property preimages are added.

A new database row starts as the same document shape without generated identity fields:

```markdown
---
title: "New task"
"Status": "Todo"
---
# Notes
```

On push, AgentFS creates the Notion row under the parent data source, reads back the assigned remote ID, and rewrites the file into the normal projected filename with generated `afs` metadata.
