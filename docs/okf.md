# Open Knowledge Format Strategy

OKF is strategically aligned with Locality because it standardizes the same
primitive Locality already exposes: directories of Markdown files with YAML
frontmatter. The non-breaking path is to treat OKF as an export and exchange
view over Locality's canonical projection, not as a replacement for `page.md`
sync files.

## Product Contract

- Existing mounts continue to expose remote pages as directories with `page.md`.
- Pull, push, Live Mode, shadows, journals, and conflict handling continue to use
  canonical Locality Markdown.
- OKF export is additive and offline: it reads mounted files and writes a
  separate bundle directory.
- OKF output preserves Locality identity metadata under extension fields so a
  future import path can map concepts back to remote entities deliberately.

## First Use Cases

1. **Portable agent context bundles**
   Export a Notion or Google Docs workspace into an OKF bundle that any agent can
   inspect with normal file tools, without needing Notion or Google credentials.

2. **Knowledge review and sharing**
   Generate a clean bundle for review, archival, or handoff while keeping the
   live mounted workspace unchanged.

3. **Indexing and retrieval**
   Feed OKF bundles into search, catalog, or retrieval pipelines that expect
   self-describing Markdown concepts.

4. **Connector-neutral demos**
   Show Locality turning app content into a vendor-neutral knowledge bundle
   without making users learn Locality's internal `page.md` shape first.

## Roadmap

### Phase 1: Export

Implemented as:

```bash
loc okf export <mounted-path> --out <empty-dir>
```

Mapping:

- `some-page/page.md` becomes `some-page.md`.
- Child pages stay under `some-page/`.
- `index.md` files are generated for progressive disclosure.
- unhydrated stubs are skipped and reported.
- `type`, `title`, `description`, `resource`, `tags`, and `timestamp` are
  populated when available.
- Locality remote IDs, source paths, connector hints, and sync timestamps are
  stored under `locality:`.

### Phase 2: Validate

Add:

```bash
loc okf validate <bundle>
```

This should check OKF conformance locally: every concept has YAML frontmatter,
every concept has a non-empty `type`, generated indexes link to existing files,
and broken links are reported as warnings rather than hard errors.

### Phase 3: Import Drafts

Add:

```bash
loc okf import <bundle> --to <mounted-dir>
```

This should create local draft pages only. It should not overwrite existing
remote-backed files unless a review command explicitly accepts the mapping.

### Phase 4: Optional OKF View

Only after export/import usage is proven, consider a read-only OKF view in the
desktop app. This should remain a view over the canonical projection, not a new
sync backend.

## Deliberate Non-Goals For Now

- No replacement of mounted `page.md` files.
- No remote writes from OKF export.
- No direct OKF Live Mode.
- No silent import overwrite of existing mounted content.
- No connector-specific OKF schema registry.
