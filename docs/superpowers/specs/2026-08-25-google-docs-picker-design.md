# Google Docs Picker Mount Design

**Goal:** Replace the folder-based, restricted-scope Google Drive integration with a flat Google Docs mount populated by documents that the user explicitly selects through Google Picker.

## Scope and consent

Google Docs OAuth requests `openid`, `email`, `profile`,
`https://www.googleapis.com/auth/documents`, and
`https://www.googleapis.com/auth/drive.file`.

The connector must not request `drive.metadata.readonly`, `drive.readonly`,
`drive.metadata`, or full `drive`. `documents` is required to read and edit a
selected document and to create a new document. `drive.file` is non-sensitive
and grants access only to files that the user explicitly selects in Google
Picker or that Locality creates.

## Mount setup and selection

Creating or reconfiguring a Google Docs mount opens Google Picker in multi-select
mode. The picker is restricted to Google Docs. Locality persists the resulting
stable document IDs as the mount's selected-document set. The user can reopen
the mount's selection UI to add or remove documents; Locality does not discover
documents in the account, traverse folders, or read Drive metadata.

## Projection and sync

Each selected document is represented directly under the mount root as a page
directory containing `page.md`. There are no projected Drive folders. The Docs
API supplies the document ID, title, body, and revision ID through
`documents.get`; the title determines the local page path. Pull and hydration
operate only on persisted selected document IDs.

Locality uses `documents.batchUpdate` for body edits and
`documents.create` for new root-level documents. Existing selected documents
cannot be renamed, moved, archived, or trashed through the mount because those
operations require Drive APIs. Locality must reject those plans before applying
any mutation. A newly created document is automatically added to the selected
document set.

## Compatibility

Existing Google Docs mounts store a Drive workspace folder in
`MountConfig.remote_root_id`. Upgrading must preserve their local projections,
shadows, pending plans, and credentials. The daemon marks those mounts as
requiring selection migration and prevents unsafe synchronization until the user
selects the document set. It must never silently delete the folder-based mount
or discard pending local work.

## Verification

Tests must assert the exact OAuth scope set and rejection of Drive metadata
scopes; Picker multi-select persistence; flat projection of only selected IDs;
Docs-only pull, hydration, body edit, and create; rejection of Drive-only
mutations; and safe opening of pre-existing folder mounts with pending local
work.
