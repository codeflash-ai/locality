# Google Docs Connector Summary

The `google-docs` connector mounts an explicit, flat selection of Google Docs.
It does not enumerate Google Drive, inspect Drive metadata, or use Drive
folders as mount boundaries.

## OAuth and selection

Google Docs OAuth uses the Locality OAuth broker and requests only:

- `openid`
- `email`
- `profile`
- `https://www.googleapis.com/auth/documents`
- `https://www.googleapis.com/auth/drive.file`

`documents` reads and writes document bodies. `drive.file` lets Google Picker
grant Locality access to Docs the user selects and lets Locality keep access to
Docs it creates. Locality does not request full Drive, Drive read-only, or any
Drive metadata scope, and it makes no Drive metadata API calls.

In Desktop, create or reconfigure a Google Docs mount with the Google Picker.
The Picker allows multi-selection and accepts only native Google Docs. Locality
persists the selected document IDs; it never persists or discovers a Drive
folder. Official Locality desktop packages include the Picker configuration for
the Locality Google Cloud project. Development builds can override it before
starting Locality:

```bash
export LOCALITY_GOOGLE_PICKER_DEVELOPER_KEY='<Google API key>'
export LOCALITY_GOOGLE_PICKER_PROJECT_NUMBER='<numeric Google Cloud project number>'
```

The project number must be numeric and belongs to the same Google Cloud project
as the Picker API key and OAuth client. These values configure Picker only; an
OAuth token is used only for the active local Picker session and is not saved in
mount settings.

For CLI or automation setup, provide one or more selected document IDs or
Google Docs URLs explicitly:

```bash
loc mount google-docs ~/Locality/google-docs-main \
  --document <id-or-url> \
  --document <id-or-url> \
  --projection plain-files
loc pull ~/Locality/google-docs-main
```

Existing folder-based Google Docs mounts are preserved as durable state but are
not opened as Drive mounts. Locality pauses them with re-selection guidance, so
pending local work is not silently discarded. Reconfigure the mount in Desktop
with Google Picker, or create a replacement CLI mount with `--document` values.

## Projection and operations

Every selected Doc appears directly at the mount root as a page directory with
`page.md`:

```text
google-docs-main/
  launch-brief/
    page.md
  roadmap/
    page.md
```

The connector supports Google Docs body reads and supported body updates. A new
root-level page directory with `page.md` creates a Google Doc; after a successful
push, its ID is added to the mount selection automatically. Use the normal
review flow:

```bash
loc status <path>
loc diff <path>
loc push <path> -y
```

Rename, move, archive/delete, and folder creation are unsupported because they
require Drive metadata or folder operations. Google Sheets, Slides, binary Drive
files, comments, suggestions, and unsupported rich Docs structures are not
editable through this connector. Unsupported rendered structures remain
push-blocking when an edit would be lossy.

## Google OAuth verification

Keep the Google Cloud Console verification scope list aligned with
`connectors/oauth-verification/google-docs.json`. Submit only the Docs and
`drive.file` scopes above. Do not submit full Drive, Drive read-only, Drive
metadata, or Docs read-only scopes.

The verification demo should show consent, Picker selection of one or more
Google Docs, body read/edit, and creation of a new root-level Doc. It should not
show Drive folder discovery, Drive metadata, rename/move/archive, or folder
creation.

## Live E2E

`tests/live_google_docs_vfs_roundtrip.sh` exercises the live Google Docs API,
CLI mount/pull/diff/push paths, `localityd`, and Linux FUSE. It requires an
already-selected scratch Google Doc ID (or a comma-separated list of IDs):

```bash
secret_ref='connection:google-docs-live'
secret_hex="$(printf '%s' "$secret_ref" | od -An -tx1 -v | tr -d ' \n')"
export LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON="$(cat "$HOME/.loc/credentials/$secret_hex")"
export LOCALITY_GOOGLE_DOCS_LIVE_DOCUMENT_IDS='scratch-doc-id-1,scratch-doc-id-2'
```

The test creates and edits a new root-level Doc, verifies both changes after
pull, and verifies the created Doc joins the selected mount. Docs API has no
trash/delete operation, so cleanup of the created scratch Doc is manual; the
script prints its ID and URL. Run it explicitly:

```bash
LOCALITY_LIVE_GOOGLE_DOCS_VFS=1 tests/live_google_docs_vfs_roundtrip.sh
```

Use the full stored credential JSON. The live harness requires `access_token`,
`oauth_broker_url`, `refresh_token_handle`, and numeric `expires_at` to exercise
broker refresh when the token expires.
