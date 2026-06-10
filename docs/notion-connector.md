# Notion Connector

`afs-notion` owns Notion API transport, DTOs, pagination, block rendering, future parse/apply behavior, and Notion-specific error mapping.

## Current Scope

The current implementation is a live-capable read and pull projection:

- `HttpNotionApi` calls the live Notion REST API with a bearer token from `NOTION_TOKEN`.
- `search_pages` can enumerate all pages shared with the integration when no root page is configured.
- root-page enumeration walks child-page blocks and projects the page tree into stable AgentFS paths.
- `fetch` retrieves page metadata and recursively retrieves paginated block children.
- fetched pages are serialized into a versioned native JSON bundle inside `NativeEntity.raw`;
- `render_native_entity` converts that native bundle into canonical Markdown plus a `ShadowDocument`;
- unsupported or lossy blocks render as `::afs{...}` directives so they retain remote identity.

The generic connector `render` method still returns only `CanonicalDocument`. The Notion connector exposes `render_native_entity` for callers that need the shadow in the same pass. A future connector SDK revision can lift that richer return type into the generic trait once another connector validates the shape.

## Live Test Hook

To test against a real page:

```sh
export NOTION_TOKEN='secret_...'
export AFS_NOTION_PAGE_ID='...'
cargo test -p afs-notion live_fetch_and_render_page_from_environment -- --ignored
```

The token must have access to the target page. Live tests are ignored by default; fixture-backed tests cover normal CI.

## Initial Block Rendering

The renderer currently supports paragraphs, headings, bulleted/numbered list items, to-dos, quotes, callouts, code blocks, dividers, child-page/database directives, and unsupported-block directives.

Nested children are fetched recursively and rendered after their parent. This preserves content and block IDs for the first read path, but it does not yet preserve every Notion nesting/layout nuance. Layout-rich blocks should stay directive-backed until the renderer can round-trip them safely.

## Path Projection

Root-page mounts use the same filename shape described in `plan.md`: `slugified-title ~shortid.md`.

For a page that also has child pages, AgentFS reserves a sibling directory with the same stem:

```text
roadmap ~aaaaaa.md
roadmap ~aaaaaa/
  design-notes ~bbbbbb.md
  tasks ~cccccc/
```

The remote ID remains the identity. The short ID starts at six hex characters and lengthens on sibling collisions, while the title slug can change without changing identity.

Database blocks currently project as directories only. Row pages, `_schema.yaml`, and `_view.csv` are still future work.
