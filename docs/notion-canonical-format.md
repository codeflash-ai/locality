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

The first renderer supports common text blocks and simple tables directly. Child pages, child databases, and unsupported/lossy blocks render as directives. This keeps the page inspectable while preserving remote block IDs for later safer round-trip support.
