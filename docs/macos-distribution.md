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

That script builds `afs`, `afsd`, and the Swift File Provider extension, stages
`AgentFSFileProvider.appex` and `agentfs-file-providerctl` under
`apps/desktop/src-tauri/macos/AgentFSFileProvider/`, stages `afs` and `afsd`
under `apps/desktop/src-tauri/macos/`, and Tauri copies those files into the
final app bundle. After the Tauri DMG is created, `build-tauri` runs
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

During onboarding, the desktop app also verifies the terminal command. For DMG
installs it creates or refreshes `/usr/local/bin/afs` as a symlink to the
embedded `Contents/MacOS/afs`, prompting for administrator permission only when
that standard PATH location is not writable. If the app is launched from the
mounted DMG volume, onboarding asks the user to move AFS to Applications before
installing the terminal command so the symlink does not point at a temporary
volume.

## Release Signing

For public direct download, the release build should be signed with a Developer
ID Application certificate and notarized. The File Provider extension must be
signed with its own entitlements before the containing app is signed. Public
macOS builds are Apple Silicon-only.

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
`APPLE_SIGNING_IDENTITY`, so the nested File Provider extension, helper, `afs`
CLI, and `afsd` sidecar are signed with the same release identity and hardened
runtime.

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

The publish script also requires an Apple Silicon host by default. Set
`PUBLISH_ALLOW_INTEL=1` only for an unsupported local experiment; public builds
should come from Apple Silicon.

The final artifact is copied to:

```text
target/release/bundle/dmg/AFS-beta-YYYYMMDD-<commit>-notarized-<arch>.dmg
```

Useful overrides:

```sh
PUBLISH_CHANNEL=release make publish
PUBLISH_DMG_NAME=AFS-beta-custom-notarized-aarch64.dmg make publish
```

## Auto-Update Artifacts

AFS uses Tauri's updater plugin for signed in-app updates. The updater signing
key is separate from Apple code signing and notarization.

Generate the updater key pair once:

```sh
npm --prefix apps/desktop run tauri -- signer generate -w ~/.tauri/afs-updater.key
```

Store the private key content in CI as `TAURI_SIGNING_PRIVATE_KEY`. If the key
has a password, store it as `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. The public key
from `~/.tauri/afs-updater.key.pub` is safe to share and must be supplied to
release builds as `TAURI_UPDATER_PUBKEY`.

Release builds enable updater artifacts when both `TAURI_UPDATER_PUBKEY` and
`TAURI_SIGNING_PRIVATE_KEY` are set:

```sh
export TAURI_UPDATER_PUBKEY="$(cat ~/.tauri/afs-updater.key.pub)"
export TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/afs-updater.key)"
export TAURI_UPDATER_ENDPOINT="https://github.com/codeflash-ai/afs/releases/latest/download/latest-macos.json"
make publish
```

The publish script copies the signed updater archive and signature to:

```text
target/release/bundle/updater/AFS-beta-YYYYMMDD-<commit>-macos-<arch>.app.tar.gz
target/release/bundle/updater/AFS-beta-YYYYMMDD-<commit>-macos-<arch>.app.tar.gz.sig
```

After uploading the updater archive to the release, render the static updater
manifest:

```sh
GITHUB_RELEASE_TAG=v0.1.0 make render-updater-manifest
```

Upload `target/release/bundle/updater/latest-macos.json` beside the updater
archive. The first public build that includes `TAURI_UPDATER_PUBKEY` is the
baseline; older builds that do not include the updater plugin still need one
manual DMG or Homebrew upgrade.

## Homebrew Cask

Homebrew should install the same notarized Apple Silicon DMG. Build and upload
the DMG to a GitHub Release, then render the cask:

```sh
HOMEBREW_RELEASE_TAG=v0.1.0 make render-homebrew-cask
```

The rendered cask is written to:

```text
target/release/homebrew/Casks/afs.rb
```

Copy that file into the CodeFlash tap, for example:

```text
codeflash-ai/homebrew-tap/Casks/afs.rb
```

The generated cask declares `depends_on arch: :arm64`, so Intel Macs are not a
supported Homebrew installation target.

## GitHub Release Workflow

The GitHub workflow in `.github/workflows/release-macos.yml` publishes the
macOS channel from a `v*` tag or manual workflow dispatch. It runs on the
GitHub-hosted `macos-15` arm64 runner, builds the notarized DMG, produces the
signed updater archive, renders `latest-macos.json`, renders `afs.rb`, creates
or updates the GitHub Release, uploads all release assets, and optionally pushes
the cask to the Homebrew tap.

Required repository secrets:

- `APPLE_CERTIFICATE_P12_BASE64`: base64-encoded Developer ID Application
  certificate exported as `.p12`.
- `APPLE_CERTIFICATE_PASSWORD`: password for the `.p12`.
- `APPLE_ID`: Apple ID for notarization.
- `APPLE_PASSWORD`: app-specific password for notarization.
- `APPLE_TEAM_ID`: Apple Developer Team ID.
- `TAURI_UPDATER_PUBKEY`: public updater signing key.
- `TAURI_SIGNING_PRIVATE_KEY`: private updater signing key.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: updater key password, if one was set.

Optional repository secret:

- `APPLE_SIGNING_IDENTITY`: exact Developer ID identity if the imported
  keychain has more than one Developer ID Application certificate.
- `HOMEBREW_TAP_TOKEN`: fine-grained token with write access to the Homebrew tap
  repo. If omitted, the workflow still uploads `afs.rb` to the GitHub Release,
  but it does not push to the tap.

Optional repository variable:

- `HOMEBREW_TAP_REPOSITORY`: defaults to `codeflash-ai/homebrew-tap`.

Release a new version by updating `apps/desktop/src-tauri/tauri.conf.json` and
`apps/desktop/package.json` to the same version, committing the change, tagging
that commit, and pushing the tag:

```sh
git tag v0.1.0
git push origin v0.1.0
```

The workflow requires the tag to match the Tauri app version exactly.

## Mac App Store Track

Mac App Store distribution should be a separate build target from the direct DMG
and Homebrew release. The App Store build needs App Sandbox enabled, App Store
signing/provisioning instead of Developer ID signing, App Store Connect metadata,
and review of the File Provider extension, embedded sidecars, and CLI install
behavior. Keep the direct DMG/Homebrew build as the fast-moving beta channel
until the sandboxed build has its own install and update story.

## Distribution Channels

Initial channel: notarized DMG direct download.

Power-user channel: Homebrew cask that installs the same notarized DMG.

Fast-moving channel: Tauri updater using the signed updater archive and
`latest-macos.json` manifest from the GitHub Release.

Later channel: Mac App Store after a sandboxed App Store build target is ready.
