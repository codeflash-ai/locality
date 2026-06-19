# Desktop Mounts Table Design

## Goal

The desktop app currently shows one selected mount even though the store can contain multiple mounts. It is also unclear that the desktop Notion mount flow updates the fixed `notion-main` mount id. The Mount page should show all mounts and make remount/update behavior explicit before it runs.

## Scope

This first version is an inventory and clarification pass. It does not add true multi-create, generated mount ids, deletion, unregister, or per-row connection switching.

## Data Model

Extend the desktop snapshot with a `mounts: MountSummary[]` field while preserving the existing `mount: MountSummary` field for Home, Pending, tray, and onboarding compatibility.

Each mount summary includes:

- `mountId`
- `connector`
- `workspaceName`
- `localPath`
- `notionUrl`
- `projection`
- `readOnly`
- `status`
- `connectionId`
- `isPrimary`

The backend builds `mounts` from `store.load_mounts()`, sorted with Notion mounts first and then by `mount_id`. The existing `mount` remains the result of the current `choose_mount` behavior.

## UI

Rename the Mount view heading to `Mounts` and make a table the primary surface.

Columns:

- Mount
- Workspace / connection
- Local path
- Projection
- Access
- Status
- Actions

Row actions:

- Open
- Copy Path
- Remount / Update

If there are no mounts, keep the current empty state and `Create Notion Folder` action.

## Remount Behavior

Desktop-created Notion mounts continue to target `notion-main` in this version. If `notion-main` already exists and the user starts the create/remount flow, the UI shows an explicit confirmation before invoking `create_workspace_mount`.

Confirmation copy:

- Title: `Update existing Notion mount?`
- Body: `This will update notion-main to use the selected folder, projection, and current Notion connection. Existing pending edits are not pushed.`
- Primary: `Update Mount`
- Secondary: `Cancel`

The table should make the fixed mount id visible so users can connect this confirmation to the existing row.

## Backend Commands

Reuse `desktop_snapshot` for the mounts table by adding `mounts`. Reuse `create_workspace_mount` for the update action. No new destructive backend command is needed.

The UI determines whether to show the update confirmation by checking for a mount summary with `mountId === "notion-main"` and `connector === "notion"`.

## Error Handling

If loading mounts fails, `desktop_snapshot` should continue returning the existing command error instead of inventing a partial state.

If `create_workspace_mount` fails during update, show the existing inline error area on the Mount page. The table remains visible with the last loaded snapshot.

If opening a path fails, show the row action error without changing selection.

## Testing

Backend tests:

- Snapshot includes every saved mount in `mounts`.
- The chosen legacy `mount` remains stable when multiple mounts exist.
- Mount summaries include access-root paths, projection labels, connection ids, and primary flags.

Frontend tests or component-level checks where available:

- Multiple sample mounts render as table rows.
- Existing `notion-main` create/remount path shows confirmation before invoking the backend.
- Row Open and Copy actions target the row path, not the legacy selected mount.

Manual verification:

- Launch the desktop app with two saved mounts and verify both appear.
- Update the existing Notion mount and verify confirmation appears before the command runs.
- Confirm Home and tray still use the primary mount without visual regressions.
