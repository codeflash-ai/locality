# Notion Workspace Root Hierarchy Design

## Context

Workspace-level Notion mounts currently project all accessible top-level pages
directly under the mount root:

```text
notion-main/
  Roadmap/
    page.md
  Personal Notes/
    page.md
```

The page-directory layout is correct for ordinary Notion pages, but the mount
root does not express the Notion placement choice between private workspace
pages and shared workspace or team-level pages. That makes root-level creation
ambiguous: a new `notion-main/New Page/page.md` file has no filesystem parent
that says whether Locality should create a private page or attempt to place it
under a shared/team workspace area.

The current public Notion API gives Locality a reliable private workspace-level
create path, but it does not expose ordinary teamspace containers as stable page
parents in the same metadata used for page hierarchy. Team-level pages are still
represented as workspace-parent pages. The design therefore introduces explicit
filesystem containers for the root-level choices Locality can represent
honestly now, while leaving room for real teamspace containers later.

## Goals

- Make workspace-root placement explicit in the mounted filesystem.
- Allow users and agents to create a new private Notion page through Locality.
- Keep existing page-directory semantics: pages are directories, and `page.md`
  is the page body.
- Preserve the remote parent invariant: the containing directory decides the
  create parent.
- Avoid inventing teamspace identities that the Notion API does not currently
  expose through normal page parent metadata.
- Cover creation and browsing behavior with focused tests.

## Non-Goals

- Do not infer teamspace membership from page titles, search order, URLs, or
  other heuristics.
- Do not create Notion teamspace pages unless a future Notion API surface gives
  Locality a stable teamspace parent.
- Do not change child-page, database, database-row, media, or `page.md`
  rendering semantics below ordinary pages.

## User-Facing Shape

Workspace-level Notion mounts get synthetic root directories:

```text
notion-main/
  Private/
    My Private Draft/
      page.md
  Workspace/
    Shared Launch Plan/
      page.md
    Engineering Team Page/
      page.md
```

`Private/` is a synthetic creation container. A new page created below it maps to
Notion's private workspace-level page creation behavior.

`Workspace/` is the synthetic browsing container for accessible pages and
databases whose Notion parent is `workspace`, including team-level pages that
the API currently represents as workspace-parent pages. It is named
`Workspace/`, not `Teamspaces/`, because Locality cannot prove the teamspace
name or teamspace parent from the current metadata.

Below those synthetic containers, existing Notion semantics remain unchanged:

```text
notion-main/
  Workspace/
    Shared Launch Plan/
      page.md
      Child Page/
        page.md
      Tasks/
        _schema.yaml
        Fix Login/
          page.md
```

## Creation Semantics

Creating a new page under `Private/` should be accepted:

```text
notion-main/
  Private/
    Scratch Idea/
      page.md
```

`loc push notion-main/Private/Scratch Idea/page.md -y` creates a private
workspace-level Notion page. The Notion create request should use the private
workspace-level create shape, meaning no page or data-source parent is supplied.

Creating a new page under `Workspace/` is not supported in this change because
Locality cannot distinguish the intended teamspace or shared workspace
placement. The validation error should explain that direct `Workspace/` creates
are ambiguous and suggest creating under `Private/` for a private page or under
an existing page for a child page.

Creating below an existing page remains supported:

```text
notion-main/
  Workspace/
    Shared Launch Plan/
      Follow-up/
        page.md
```

That still creates a child page under `Shared Launch Plan`, because the
containing directory has a real Notion page identity.

If a future Notion API exposes stable teamspace containers, Locality can add:

```text
notion-main/
  Teamspaces/
    Engineering/
      New Team Page/
        page.md
```

until then, `Teamspaces/` must not be emitted or accepted as a real parent.

## Data Model

Represent synthetic Notion root containers as source-root entities in local
state. They are not remote Notion pages and should not hydrate, push as pages,
or appear as remote observations.

Required synthetic entity properties:

- connector: `notion`
- kind: `Directory`
- remote IDs reserved under a connector namespace, for example:
  - `notion-root:private`
  - `notion-root:workspace`
- paths:
  - `Private`
  - `Workspace`
- titles:
  - `Private`
  - `Workspace`

The synthetic IDs only need to be stable within a mount and must never be sent
to Notion as page IDs.

Push planning needs one additional parent concept for creates:

- real page parent: existing behavior, creates `parent[type]=page_id`;
- database parent: existing behavior, creates under a data source;
- synthetic private root parent: new behavior, creates a private workspace-level
  page with no page/data-source parent;
- synthetic workspace root parent: validation-only; direct create is rejected as
  ambiguous.

## Projection And Listing

Workspace-level enumeration should emit `Private` and `Workspace` directories at
the mount root.

Pages/databases that previously appeared directly at the mount root because
their parent was `workspace` should now appear under `Workspace/`. This includes
team-level pages represented by Notion as workspace-parent pages.

Fallback pages discovered by search whose accessible parent is not in the local
set should also appear under `Workspace/`, because Locality cannot prove a more
specific root container.

Root-page mounts should not get `Private/` or `Workspace/`; they already have a
real configured root page, and direct children belong below that page.

Virtual filesystem listings should expose the same synthetic directories as
plain-file pulls. The synthetic directories are folders only. Opening them lists
children; they do not contain `page.md`.

## Upgrade Repair

Existing workspace-level Notion mounts created before the synthetic `Private/`
and `Workspace/` roots shipped must repair persisted local state on reopen.
That repair must move legacy projection files and virtual content-cache files
from the old mount-root layout into the new `Workspace/...` layout before the
store rewrites persisted projected paths, so upgraded mounts keep their hydrated
or dirty local content attached to the correct entities.

## Push And Apply

Push preparation should identify `Private/<name>/page.md` as a create whose
parent is the synthetic private root. The produced `PushOperation::CreateEntity`
must preserve enough parent information for the Notion apply layer to build a
private workspace-level create request.

The existing `PushOperation::CreateEntity` has `parent_id` and `parent_kind`.
Extend it with a connector-neutral parent scope enum so the apply layer can
distinguish real remote parents from synthetic root containers. Do not encode
the private root as a fake remote page ID; synthetic IDs must never be sent to
Notion.

`loc diff` and `loc push` should reject `Workspace/<name>/page.md` creates with a
structured validation issue before any Notion request.

## Error Handling

If a user tries to create under `Workspace/`:

```text
Workspace/New Team Page/page.md
```

the error should be explicit:

```text
New root workspace pages are ambiguous because Notion does not expose a stable
teamspace parent through this API. Create under Private/ for a private page, or
create below an existing page that Locality can use as the parent.
```

If a user tries to edit `Private/` or `Workspace/` as files, normal filesystem
behavior should reject that because they are directories.

If a synthetic root directory collides with a real Notion page title, the
synthetic directory wins at the mount root. The real page is projected under
`Workspace/Private/` or `Workspace/Workspace/` according to normal sibling
allocation inside the `Workspace/` container.

## Tests

Add tests before implementation:

- Notion workspace enumeration emits `Private/` and `Workspace/` at the mount
  root.
- Workspace-parent pages/databases are projected under `Workspace/`.
- Root-page mounts do not emit synthetic root containers.
- `loc diff` or push preparation accepts `Private/New Page/page.md` as a
  private workspace-level create plan.
- The Notion apply layer builds a create-page request without `parent` for the
  private root create.
- `Workspace/New Page/page.md` is rejected before apply with an ambiguity
  validation issue.
- Creating below an existing page under `Workspace/Page/Child/page.md` still
  creates with the existing page as parent.
- Virtual projection children for the mount root include the synthetic
  directories and no `page.md` for them.

Live verification should use scratch content:

- create a private page through `Private/<scratch>/page.md`;
- verify through the Notion API that the page was created with a workspace-level
  private parent;
- archive the scratch page;
- if a real teamspace fixture is available, verify an existing team-level page
  appears under `Workspace/` rather than pretending Locality knows a teamspace
  name.

## Documentation

Update:

- `docs/notion-connector.md`
- `docs/cli.md`
- `templates/mount/AGENTS.md`
- Linux FUSE and virtual projection docs if they describe mount-root browsing

The core guidance should be:

```text
Workspace Notion mounts expose root placement directories. Create under
Private/ for a private workspace-level page. Existing workspace/team-level pages
appear under Workspace/ until Notion exposes stable teamspace parents.
```

## Rollout

1. Land synthetic root containers and tests for workspace mounts.
2. Land private-page create planning and Notion apply support.
3. Update docs and agent guidance.
4. Add live ignored verification for private creation.
5. Later, add true `Teamspaces/` only when Notion exposes stable teamspace
   parent metadata or another reliable source of truth.
