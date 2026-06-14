# Notion Manual Review Compatibility Matrix

This is a temporary manual-verification matrix for the live Notion corpus created
for PR review. After manual verification, remove the review-only filesystem and
Notion links before publishing a user-facing compatibility matrix.

Review corpus:

- Notion root: [AFS Manual Review 20260613-230413](https://www.notion.so/37e3545ddc6281b8bc8ac1ebd27c3a67)
- Mounted root: `/tmp/afs-manual-review-20260613-230413`
- Clean review aliases: `/tmp/afs-manual-review-links-20260613-230413`
- Isolated AFS state: `/tmp/afs-manual-review-state-20260613-230413`

The review aliases are symlinks into the mounted root. They are easier to open
because they do not contain projection suffixes or spaces. Editing through an
alias edits the mounted file. For push/status verification, use the mounted root
or ask the agent to use the isolated state directory above.

## Manual Review Matrix

| Notion feature | Status | Filesystem review URL | Notion review URL | Remarks |
|---|---|---|---|---|
| Page root projection | Supported | [FS](file:///tmp/afs-manual-review-links-20260613-230413/00-root.md) | [Notion](https://www.notion.so/37e3545ddc6281b8bc8ac1ebd27c3a67) | Root page hydrates as Markdown and enumerates child pages/databases below a projected folder. |
| Paragraph block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Edit normal paragraph text and push. |
| Rich-text annotations | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Covers bold, italic, strikethrough, underline, and inline code. |
| External rich-text link | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as a Markdown link. URL and label edits should push. |
| Inline equation rich text | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as inline math. |
| Page mention rich text | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Existing mentions render as Notion links. New typed mentions can be written as `@page(<notion-page-id>)`. |
| Database mention rich text | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Existing mentions render as Notion links. New typed mentions can be written as `@database(<notion-database-id>)`. |
| Date mention rich text | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Existing dates render readably. New typed dates can be written as `@date(2026-06-14)`. |
| User mention rich text | Supported read/write by ID | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Existing mention renders as readable text. New typed users require `@user(<notion-user-id>)`. |
| Link-preview rich-text mention | Read-only | [FS](file:///tmp/afs-manual-review-links-20260613-230413/02-links-mentions-url-blocks.md) | [Notion](https://www.notion.so/37e3545ddc6281b99f66eb3e46b4f16d) | Live Notion write validation rejected `mention.link_preview`; AFS should not synthesize it yet. |
| Heading 1 block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as `#`. |
| Heading 2 block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as `##`. |
| Heading 3 block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as `###`. |
| Heading 4 block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as `####`. |
| Bulleted list item block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | One Notion block per Markdown list item. |
| Numbered list item block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | One Notion block per Markdown numbered item. |
| To-do block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Toggle checkbox state using Markdown task syntax. |
| Quote block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as Markdown blockquote. |
| Callout block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as `[!NOTE]` style quote. Emoji/color are not the main edit surface yet. |
| Code block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Language and content round-trip for simple fenced code blocks. |
| Divider block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as Markdown horizontal rule. |
| Display equation block | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Renders as display math. |
| Table block and table rows | Supported with stable width/header mode | [FS](file:///tmp/afs-manual-review-links-20260613-230413/01-editable-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62815d937fc8c5e980bbe4) | Cell edits, row add, and trailing row delete are supported. Width/header-mode changes remain blocked. |
| Bookmark block | Supported for existing URL blocks | [FS](file:///tmp/afs-manual-review-links-20260613-230413/02-links-mentions-url-blocks.md) | [Notion](https://www.notion.so/37e3545ddc6281b99f66eb3e46b4f16d) | Renders as Markdown link. Caption and URL edits push. |
| Embed block | Supported for existing URL blocks | [FS](file:///tmp/afs-manual-review-links-20260613-230413/02-links-mentions-url-blocks.md) | [Notion](https://www.notion.so/37e3545ddc6281b99f66eb3e46b4f16d) | Renders as Markdown link. Caption and URL edits push. |
| Link-preview block | Read-only | [FS](file:///tmp/afs-manual-review-links-20260613-230413/02-links-mentions-url-blocks.md) | [Notion](https://www.notion.so/37e3545ddc6281b99f66eb3e46b4f16d) | Not created in this corpus because the live create-page API rejected link-preview child blocks. |
| `link_to_page` page target block | Read supported; target retarget blocked | [FS](file:///tmp/afs-manual-review-links-20260613-230413/02-links-mentions-url-blocks.md) | [Notion](https://www.notion.so/37e3545ddc6281b99f66eb3e46b4f16d) | Renders as Markdown link. Direct target PATCH was a live API no-op, so retargeting is guarded. |
| `link_to_page` database target block | Read supported; target retarget blocked | [FS](file:///tmp/afs-manual-review-links-20260613-230413/02-links-mentions-url-blocks.md) | [Notion](https://www.notion.so/37e3545ddc6281b99f66eb3e46b4f16d) | Renders as Markdown link. Direct retargeting is deferred pending undo-aware block replacement. |
| Image block with external URL | Supported for existing URL blocks | [FS](file:///tmp/afs-manual-review-links-20260613-230413/03-media-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62813b95d7c59cde0fe940) | Renders as Markdown image. Existing URL/caption edits push. Local image copy is in the mount `media/` directory. |
| Video block with external URL | Supported for existing URL blocks | [FS](file:///tmp/afs-manual-review-links-20260613-230413/03-media-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62813b95d7c59cde0fe940) | Renders as Markdown link. |
| File block with external URL | Supported for existing URL blocks | [FS](file:///tmp/afs-manual-review-links-20260613-230413/03-media-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62813b95d7c59cde0fe940) | Renders as Markdown link. |
| PDF block with external URL | Supported for existing URL blocks | [FS](file:///tmp/afs-manual-review-links-20260613-230413/03-media-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62813b95d7c59cde0fe940) | Renders as Markdown link. |
| Audio block with external URL | Supported for existing URL blocks | [FS](file:///tmp/afs-manual-review-links-20260613-230413/03-media-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62813b95d7c59cde0fe940) | Renders as Markdown link. |
| Media upload/local file upload | Unsupported | [FS](file:///tmp/afs-manual-review-links-20260613-230413/03-media-blocks.md) | [Notion](https://www.notion.so/37e3545ddc62813b95d7c59cde0fe940) | Current writer supports external URL rewrites, not uploading local files to Notion. |
| Toggle block | Read-only wrapper with readable children | [FS](file:///tmp/afs-manual-review-links-20260613-230413/04-nested-read-only-directives.md) | [Notion](https://www.notion.so/37e3545ddc6281649a3cc1a8aa4ab294) | Toggle summary renders as a list item; nested child content stays visible. Wrapper semantics are guarded. |
| Column list block | Read-only directive | [FS](file:///tmp/afs-manual-review-links-20260613-230413/04-nested-read-only-directives.md) | [Notion](https://www.notion.so/37e3545ddc6281649a3cc1a8aa4ab294) | Layout wrapper renders as AFS directive; column child content remains visible below it. |
| Column block | Read-only directive | [FS](file:///tmp/afs-manual-review-links-20260613-230413/04-nested-read-only-directives.md) | [Notion](https://www.notion.so/37e3545ddc6281649a3cc1a8aa4ab294) | Layout semantics are not editable as Markdown yet. |
| Table of contents block | Unsupported write; directive-backed read | [FS](file:///tmp/afs-manual-review-links-20260613-230413/04-nested-read-only-directives.md) | [Notion](https://www.notion.so/37e3545ddc6281649a3cc1a8aa4ab294) | Generated navigation block; no meaningful Markdown edit surface. |
| Breadcrumb block | Unsupported write; directive-backed read | [FS](file:///tmp/afs-manual-review-links-20260613-230413/04-nested-read-only-directives.md) | [Notion](https://www.notion.so/37e3545ddc6281649a3cc1a8aa4ab294) | Generated navigation block; no meaningful Markdown edit surface. |
| Child page block and nested page enumeration | Supported read/enumeration | [FS](file:///tmp/afs-manual-review-links-20260613-230413/04-nested-child-page.md) | [Notion](https://www.notion.so/37e3545ddc6281509016d3599cf6abd3) | Child page appears as a nested Markdown file. Creating child pages from arbitrary block edits is not supported. |
| Child database block and nested database enumeration | Supported read/enumeration | [FS](file:///tmp/afs-manual-review-links-20260613-230413/04-nested-read-only-directives.md) | [Notion](https://www.notion.so/2a7caadf22bb49e2bb4fe048fc6bd673) | Child database appears as a nested directory with `_schema.yaml`. Database creation is not a Markdown block edit. |
| Database directory projection | Supported read/enumeration | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-database-properties-schema.yaml) | [Notion](https://www.notion.so/e454dddb2e20443585b699035c03ccfe) | Database projects to a directory; rows project as Markdown files. |
| Data source `_schema.yaml` | Supported read and validation source | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-database-properties-schema.yaml) | [Notion](https://www.notion.so/e454dddb2e20443585b699035c03ccfe) | Schema file is used for database row property validation. |
| Database row page body | Supported read/write/create row | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Edit body Markdown and supported frontmatter properties. Creating a new Markdown file under the database directory creates a row. |
| Title property | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `title`. |
| Rich-text property | Supported read/write with inline Markdown | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `Notes` preserves supported inline Markdown, links, and typed mentions. |
| Number property | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `Points`. |
| Select property | Supported read/write with schema validation | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `Status`; option must exist in `_schema.yaml`. |
| Status property | Supported read/write with schema validation | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `State`; option must exist in `_schema.yaml`. |
| Multi-select property | Supported read/write with schema validation | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `Tags`; options must exist in `_schema.yaml`. |
| Checkbox property | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `Done`. |
| Date property | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `Due`; supports scalar or structured range shape. |
| URL property | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `URL`. |
| Email property | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `Email`. |
| Phone property | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter `Phone`. |
| Files property with external URLs | Supported read/write | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter accepts `Name <https://...>` or raw HTTPS entries. Hosted/uploaded file ownership remains read-only. |
| People property | Supported read/write by explicit user ID | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter accepts Notion user IDs or `Name <id>`. Name/email lookup is deferred. |
| Relation property | Supported read/write by explicit page ID | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-property-row-edit-me.md) | [Notion](https://www.notion.so/37e3545ddc62819599fedc91b5143686) | Frontmatter accepts related page IDs. Path/title resolution is deferred. |
| Formula, rollup, created/edited metadata, unique ID, verification, button properties | Read-only or unsupported writes | [FS](file:///tmp/afs-manual-review-links-20260613-230413/05-database-properties-schema.yaml) | [Notion](https://www.notion.so/e454dddb2e20443585b699035c03ccfe) | Computed and Notion-managed properties are not writable by AFS. This corpus focuses on writable properties. |

## Local Push Helper

If you make local edits and want to push through the same isolated test state:

```bash
AFS_STATE_DIR=/tmp/afs-manual-review-state-20260613-230413 \
NOTION_TOKEN=<token> \
/Users/saurabh/afs-notion-cyclic-e2e/target/debug/afs push \
/tmp/afs-manual-review-20260613-230413 --yes
```

For manual review, it is usually better to ask the agent to run the push and
verification so the token does not need to be re-pasted.
