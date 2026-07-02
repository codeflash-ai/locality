# Windows Cloud Files Initial Placeholder Seeding

## Problem

After a fresh Windows install, Locality can import Notion metadata and register
the shared Cloud Files root, but `C:\Users\vm-user\Locality\notion` may show
only connector guidance files until the Cloud Files provider is restarted.

Observed evidence:

- `loc status C:\Users\vm-user\Locality\notion --json` reported 306 Notion
  entries, all synced and mostly stubbed.
- The visible Cloud Files directory initially contained only `AGENTS.md` and
  `CLAUDE.md`.
- `loc pull C:\Users\vm-user\Locality\notion\company\page.md` hydrated daemon
  state but did not create the visible path.
- `loc file-provider restart notion-main` immediately made top-level Notion
  folders visible.

The product should not require users or the desktop app to restart the provider
after first connection.

## Goal

On first provider startup, the Windows Cloud Files projection should create the
shared root mount-point placeholder and the mount point's immediate child
placeholders, so users can browse `Locality\notion` immediately after install.

## Non-Goals

- Do not change Notion enumeration, hydration, or push/pull semantics.
- Do not make the daemon write Windows Cloud Files placeholders directly.
- Do not add a desktop workaround that restarts the provider after mounting.
- Do not eagerly hydrate page bodies; the fix is metadata-only placeholder
  creation.

## Design

Fix the Windows Cloud Files helper startup seeding path in
`platform/windows/locality-cloud-files/src/main.rs`.

`seed_root_placeholders` already asks the daemon for shared-root children and
creates placeholders in the sync root. For a shared root, those children are
connector mount-point folders such as `notion`. The helper currently only seeds
children of mount-point directories that already exist at the time it scans.
On fresh startup, the mount-point placeholder has just been created and is not
treated as a child seed target until a later provider restart.

Update the startup seeding logic so folder placeholders created or already
present in the root batch are considered seed targets in the same run. For each
mount-point folder target, call `context.children(<wrapped mount-point id>)` and
create those children inside the mount-point directory. This should produce
visible placeholders such as `company`, `engineering-wiki`, and `tech` without
hydrating page contents.

The behavior should remain recursive only for directories that are visible
seed targets. It should not enumerate the whole workspace unboundedly at startup.
The initial requirement is to make immediate Notion mount children visible.

## Error Handling

If nested seeding fails, provider startup should fail with the existing helper
error path rather than silently claiming a healthy projection. This keeps
`loc doctor` and provider logs useful for install diagnostics.

If a placeholder already exists, keep the current idempotent behavior and skip
creation for that path.

## Testing

Add focused unit coverage in the Windows Cloud Files helper for seed-target
collection:

- A shared root containing a mount-point folder item should schedule that
  mount-point directory for child placeholder creation even when the directory
  was just created by the root seed batch.
- Existing child directory placeholders should remain idempotent.

If practical, add or update a projection contract test to assert that shared
Windows Cloud Files startup seeding creates mount-point child placeholders from
daemon metadata without requiring a provider restart.

## Compatibility

No persisted schema change is required. This only changes how the existing
Windows Cloud Files provider materializes daemon metadata into visible
placeholders during startup.
