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

For local release testing, prefer environment variables over hardcoding the
production identity in `tauri.conf.json`:

```sh
export APPLE_SIGNING_IDENTITY="Developer ID Application: Example, Inc. (TEAMID)"
export APPLE_ID="developer@example.com"
export APPLE_PASSWORD="app-specific-password"
export APPLE_TEAM_ID="TEAMID"
make build-tauri
```

`tauri.conf.json` uses `signingIdentity: "-"` as the checked-in default so local
developer builds are ad-hoc signed and can pass local `codesign --verify`
without requiring every contributor to have CodeFlash's Developer ID
certificate. Release automation should override that default with the real
Developer ID identity. The File Provider staging script also reads
`APPLE_SIGNING_IDENTITY`, so the nested File Provider extension, helper, and
`afsd` sidecar are signed with the same release identity and hardened runtime.

Recommended release sequence:

```sh
make setup
make build-tauri
DMG="$(find target/release/bundle/dmg -maxdepth 1 -name 'AFS_*.dmg' | sort | tail -n 1)"
xcrun notarytool submit "$DMG" --wait \
  --apple-id "$APPLE_ID" \
  --password "$APPLE_PASSWORD" \
  --team-id "$APPLE_TEAM_ID"
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"
spctl --assess --type open --context context:primary-signature --verbose "$DMG"
```

The exact production signing script still needs a final entitlement review
before automated releases.

## Distribution Channels

Initial channel: notarized DMG direct download.

Power-user channel: Homebrew cask that installs the same notarized DMG.

Later channel: Tauri updater for in-app update checks after the signing and
release hosting flow is stable.
