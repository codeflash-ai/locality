# macOS Distribution

AFS ships on macOS as a Tauri app bundle with the AgentFS File Provider
extension embedded in `Contents/PlugIns`.

## Local Development

Start the desktop app from the repo root:

```sh
make setup
make dev-tauri
```

Start the daemon manually when testing CLI or File Provider behavior outside the
desktop app:

```sh
make run-daemon
```

## Local Package Build

Build unsigned local `.app` and `.dmg` artifacts:

```sh
make build-tauri
```

The Tauri pre-bundle hook runs:

```sh
apps/desktop/scripts/prepare-macos-file-provider.sh
```

That script builds `afsd` plus the Swift File Provider extension, stages
`AgentFSFileProvider.appex` and `agentfs-file-providerctl` under
`apps/desktop/src-tauri/macos/AgentFSFileProvider/`, stages `afsd` under
`apps/desktop/src-tauri/macos/`, and Tauri copies those files into the final
app bundle. After the Tauri DMG is created, `build-tauri` runs
`apps/desktop/scripts/postprocess-dmg-volume-icon.sh` so the mounted installer
volume uses a disk-style AFS icon instead of the application icon.

Expected local artifacts:

```text
target/release/bundle/macos/AFS.app
target/release/bundle/dmg/*.dmg
```

## Beta Upgrade State

During early beta builds, the desktop app treats an existing `~/.afs/state.sqlite3`
from a different build as potentially incompatible local state. On first launch
of a new build, AFS prompts the user to reset local AFS state before onboarding
continues. The reset stops `afsd`, unregisters File Provider domains, removes
AFS metadata/cache/support state, and clears connector credentials. It does not
delete user-visible local folders or documents.

The same reset is available in the app under **Settings > Developer > Reset
Local State**. This is intentionally a beta-era escape hatch; longer-term
releases should use stable SQLite migrations and explicit state migrations
instead of asking users to reset.

The desktop app also checks the running `afsd` build metadata before reusing a
daemon. If the daemon does not report the same build ID as the app bundle, or if
it is old enough not to report build metadata, the app stops it and starts the
embedded `Contents/MacOS/afsd` from the current app bundle.

## Release Signing

For public direct download, the release build should be signed with a Developer
ID Application certificate and notarized. The File Provider extension must be
signed with its own entitlements before the containing app is signed.

Required Apple-side setup:

- Developer ID Application certificate installed locally or available in CI.
- App IDs and entitlements for `ai.codeflash.afs` and
  `ai.codeflash.afs.AgentFS.FileProvider`.
- Application group `group.ai.codeflash.afs`.
- Notary credentials, preferably an App Store Connect API key in CI.

Find the local signing identity:

```sh
security find-identity -v -p codesigning
```

Use the `Developer ID Application: ... (TEAMID)` identity. If the command prints
`0 valid identities found`, install the Developer ID Application certificate
from the Apple Developer account into the login keychain first.

For local release testing, use the publish target:

```sh
make publish
```

`tauri.conf.json` uses `signingIdentity: "-"` as the checked-in default so local
developer builds are ad-hoc signed and can pass local `codesign --verify`
without requiring every contributor to have CodeFlash's Developer ID
certificate. `make publish` overrides that default with a Developer ID identity.
If exactly one `Developer ID Application` identity is installed locally, the
script uses it automatically. Otherwise set:

```sh
export APPLE_SIGNING_IDENTITY="Developer ID Application: Example, Inc. (TEAMID)"
```

The File Provider staging script also reads
`APPLE_SIGNING_IDENTITY`, so the nested File Provider extension, helper, and
`afsd` sidecar are signed with the same release identity and hardened runtime.

Notarization uses a keychain profile named `afs-notary` by default:

```sh
xcrun notarytool store-credentials afs-notary \
  --apple-id "$APPLE_ID" \
  --password "$APPLE_PASSWORD" \
  --team-id "$APPLE_TEAM_ID"
```

Set `APPLE_NOTARY_KEYCHAIN_PROFILE` or `NOTARY_KEYCHAIN_PROFILE` to use a
different profile. If no keychain profile is available, `make publish` falls
back to `APPLE_ID`, `APPLE_PASSWORD`, and `APPLE_TEAM_ID` from the environment.

The publish script requires a clean git working tree by default because the
embedded daemon build ID is derived from `HEAD`. Use `PUBLISH_ALLOW_DIRTY=1`
only for local throwaway builds.

The final artifact is copied to:

```text
target/release/bundle/dmg/AFS-beta-YYYYMMDD-<commit>-notarized-<arch>.dmg
```

Useful overrides:

```sh
PUBLISH_CHANNEL=release make publish
PUBLISH_DMG_NAME=AFS-beta-custom-notarized-aarch64.dmg make publish
```

## Distribution Channels

Initial channel: notarized DMG direct download.

Power-user channel: Homebrew cask that installs the same notarized DMG.

Later channel: Tauri updater for in-app update checks after the signing and
release hosting flow is stable.
