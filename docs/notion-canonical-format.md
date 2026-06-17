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

The first renderer supports common text blocks, richer inline text, display equations, simple tables, bookmark/embed/link-preview URL blocks, child-page links, and file-like media blocks. Inline bold, italic, strikethrough, code, external links, date mentions, page/database mentions, link previews, and equations use ordinary Markdown or small HTML fallbacks when Markdown has no native equivalent. Child pages render as normal Markdown links whose URL contains the stable Notion page ID, for example `[Design Notes](https://www.notion.so/...)`; AFS can use that URL to locate the mounted child page, and the child page itself is edited through its own Markdown file. Child databases, toggles, synced blocks, column layouts, tabs, meeting notes, AI/custom blocks, URL-less media payloads, and unsupported/lossy blocks render as directives. This keeps the page inspectable while preserving remote block IDs for later safer round-trip support.

Media blocks with a Notion `file.url` or `external.url` render as ordinary Markdown. Images use image syntax, while other file-like blocks use links. When the renderer knows the page's filesystem path, image links point at the local `.afs/media/` copy instead of the remote Notion/S3 URL:

```markdown
![Architecture diagram](../.afs/media/roadmap/image-0123456789ab.png)
[Design brief](https://...)
```

Filesystem-aware pull, hydration, and post-push reconcile paths download image files into the mount-level `.afs/media/` directory so agents can open a local copy without cluttering the Markdown page directory or colliding with a projected Notion page named `media`. AFS also writes `.afs/media/manifest.json`, which records the image block ID, source URL, local path, size, and SHA-256 checksum used to detect binary edits. URL-less media payloads still render as directives, for example `::afs{id=image-id type=image title="Architecture diagram"}`.

The first writer supports block bodies whose Markdown shape maps to one Notion block or a guarded existing Notion table: paragraphs, headings, single list items, to-dos, quotes, code fences, dividers, display equations, existing stable-width/header-mode tables including row add/delete, existing bookmark/embed URL blocks, and existing URL-backed media blocks. Remote URL media edits still write external URLs for non-image media; local image links backed by `.afs/media/manifest.json` plan as image uploads when the Markdown caption or image bytes change. Appending new media blocks and non-image uploads are deferred. It also parses the rich inline Markdown emitted by the renderer for bold, italic, strikethrough, underline, code, external links, equations, Notion page links, database links whose target ID matches a rendered database mention, explicit page/database mentions written as `@page(<notion-page-id>)` and `@database(<notion-database-id>)`, explicit date mentions written as `@date(2026-06-14)` or `@date(2026-06-14 to 2026-06-21, tz=America/Chicago)`, explicit user mentions written as `@user(<notion-user-id>)`, and legacy `afs://` page links. Unchanged preimage mentions, such as existing date/user mentions, are preserved during block updates; unsupported inline shapes fail rather than being flattened silently.

## Database Rows

A Notion database row renders as the same page document shape with row properties flattened into frontmatter keys. Rich-text properties use the same inline Markdown contract as page bodies, so annotations, links, equations, and supported explicit mention syntax can be edited from frontmatter:

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

Property writes are planned by comparing edited frontmatter against the shadow frontmatter captured during the last render. The Notion writer currently applies title, rich text, number, select, status, multi-select, checkbox, date, URL, email, phone, external file URL, people, and relation properties. File entries use either `https://...` or `Name <https://...>` frontmatter list values and write Notion external file objects. People entries use Notion user IDs or `Name <user-id>` strings. Relation entries use Notion page IDs as strings or YAML lists. Read-only, computed, or identity-backed property classes such as formula, rollup, created/edited timestamps, created/edited users, unique ID, and verification remain read-side only until schema validation and richer property preimages are added.

A new database row starts as the same document shape without generated identity fields:

```markdown
---
title: "New task"
"Status": "Todo"
---
# Notes
```

On push, AgentFS creates the Notion row under the parent data source, reads back the assigned remote ID, and rewrites the file into the normal projected filename with generated `afs` metadata.
