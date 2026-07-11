# macOS Distribution

Locality ships on macOS as a Tauri app bundle with the Locality File Provider
extension embedded in `Contents/PlugIns`.

## Local Development

Start the desktop app from the repo root:

```sh
make setup
make dev-tauri
```

The Tauri dev command runs `apps/desktop/scripts/prepare-dev-sidecars.mjs`
before starting Vite. That script builds fresh debug `loc` and `localityd` binaries
so the desktop app does not restart into a stale daemon from an earlier commit.

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

That script builds `loc`, `localityd`, and the Swift File Provider extension, stages
`LocalityFileProvider.appex` and `locality-file-providerctl` under
`apps/desktop/src-tauri/macos/LocalityFileProvider/`, stages `loc` and `localityd`
under `apps/desktop/src-tauri/macos/`, and Tauri copies those files into the
final app bundle. The File Provider host and extension Info.plists both declare
the shared-root mount logo through `CFBundleIconFile` for full-size bundle icon
surfaces and `CFBundleSymbolName` for Finder's tintable sidebar glyph. The build
script copies `locality-mount-logo.icns` into both bundle resource directories.
Tauri also packages the same ICNS under the shipped app's `Contents/Resources`
so the containing host has the resource available. Existing
`~/Library/CloudStorage/Locality` domains may need unregister/register or a
local File Provider reset before Finder drops a cached provider icon. After the
Tauri DMG is created, `build-tauri` runs
`apps/desktop/scripts/postprocess-dmg-volume-icon.sh` so the mounted installer
volume uses a disk-style Locality icon instead of the application icon. The DMG
also carries a Finder background and icon layout that presents `Locality.app` on
the left, the Applications folder on the right, and install guidance for dragging
Locality into Applications.

Expected local artifacts:

```text
target/release/bundle/macos/Locality.app
target/release/bundle/dmg/*.dmg
```

## Upgrade State

The desktop app records the currently installed app and daemon build metadata,
but an existing `~/.loc/state.sqlite3` from a different build no longer blocks
onboarding or asks the user to reset local state. Durable state compatibility is
owned by SQLite schema and state-component migrations in `locality-store`; ordinary
upgrades should continue without a user-visible state step.

A destructive reset remains available in the app under **Settings > Developer >
Reset Local State** and from the terminal as `loc reset --yes` for explicit
repair/debugging. The reset stops `localityd`, unregisters File Provider
domains, removes Locality metadata/cache/support state, and clears connector
credentials. It does not delete user-visible local folders or documents.

The desktop app also checks the running `localityd` build metadata before reusing a
daemon. If the daemon does not report the same build ID as the app bundle, or if
it is old enough not to report build metadata, the app stops it and starts the
embedded `Contents/MacOS/localityd` from the current app bundle.

Direct-download builds also check the Tauri updater manifest on launch. The
macOS release workflow points packaged apps at
`https://github.com/codeflash-ai/locality/releases/latest/download/latest-macos.json`.
The app checks that manifest in the background, downloads an available updater
archive without blocking startup, and shows the restart/install action in the
desktop sidebar when the main window is visible. If Locality is running in the
background with no visible main window, the downloaded update can install and
relaunch automatically only after the daemon debug queue reports no active or
queued work and Live Mode is not syncing. The app also schedules a native
relaunch fallback before the updater install begins so LaunchServices opens the
updated `.app` even if the old process exits during installer handoff. The
relaunch runs the same `localityd` build validation described above before
normal desktop work resumes.

During onboarding, the desktop app also verifies the terminal command. For DMG
installs it creates or refreshes `/usr/local/bin/loc` as a symlink to the
embedded `Contents/MacOS/loc`, prompting for administrator permission only when
that standard PATH location is not writable. If the app is launched from the
mounted DMG volume, onboarding asks the user to move Locality to Applications before
installing the terminal command so the symlink does not point at a temporary
volume.

## Uninstall Cleanup

macOS does not run an uninstall hook when a user deletes a DMG-installed
`Locality.app`; dragging the app to Trash is only a filesystem delete. Before
deleting a direct-download app, users should open **Settings > Developer >
Prepare for Uninstall**. That action stops `localityd`, unregisters File
Provider state, clears Locality local state and credentials, removes
Locality-managed agent guidance and MCP `loc` entries, removes the LaunchAgent,
and removes the terminal `loc` symlink when it points at the app bundle.

The desktop binary also supports a hidden non-UI cleanup entry point:

```sh
/Applications/Locality.app/Contents/MacOS/Locality --prepare-uninstall
```

The generated Homebrew cask runs that same entry point in its `uninstall`
stanza before removing the app. The `zap` stanza remains available for users who
want Homebrew to remove residual Locality state paths.

## Release Signing

For public direct download, the release build should be signed with a Developer
ID Application certificate and notarized. The File Provider extension must be
signed with its own entitlements before the containing app is signed. Public
macOS builds are Apple Silicon-only.

Required Apple-side setup:

- Developer ID Application certificate installed locally or available in CI.
- App IDs and entitlements for `ai.codeflash.locality` and
  `ai.codeflash.locality.Locality.FileProvider`.
- Application group `C484HB7Q6S.group.ai.codeflash.locality`.
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
`APPLE_SIGNING_IDENTITY`, so the nested File Provider extension, helper, `loc`
CLI, and `localityd` sidecar are signed with the same release identity and hardened
runtime.

Notarization uses a keychain profile named `loc-notary` by default:

```sh
xcrun notarytool store-credentials loc-notary \
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
target/release/bundle/dmg/Locality-beta-YYYYMMDD-<commit>-notarized-<arch>.dmg
```

For local packaging tests that should sign the app but skip Apple notarization,
use:

```sh
make publish-unnotarized
```

This sets `PUBLISH_SKIP_NOTARIZATION=1`, skips notary credential lookup,
`notarytool submit`, stapling, and notarization validation, and writes:

```text
target/release/bundle/dmg/Locality-beta-YYYYMMDD-<commit>-unnotarized-<arch>.dmg
```

If `APPLE_SIGNING_IDENTITY` is set, or exactly one Developer ID Application
identity is installed, `make publish-unnotarized` signs with that identity.
Otherwise it uses ad-hoc signing for local-only validation.

Use `make publish` for public direct-download and Homebrew artifacts.

Useful overrides:

```sh
PUBLISH_CHANNEL=release make publish
PUBLISH_DMG_NAME=Locality-beta-custom-notarized-aarch64.dmg make publish
```

## Auto-Update Artifacts

Locality uses Tauri's updater plugin for signed in-app updates. The updater signing
key is separate from Apple code signing and notarization.

Generate the updater key pair once:

```sh
npm --prefix apps/desktop run tauri -- signer generate -w ~/.tauri/loc-updater.key
```

Store the private key content in CI as `TAURI_SIGNING_PRIVATE_KEY`. If the key
has a password, store it as `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. The public key
from `~/.tauri/loc-updater.key.pub` is safe to share and must be supplied to
release builds as `TAURI_UPDATER_PUBKEY`.

Release builds enable updater artifacts when both `TAURI_UPDATER_PUBKEY` and
`TAURI_SIGNING_PRIVATE_KEY` are set:

```sh
export TAURI_UPDATER_PUBKEY="$(cat ~/.tauri/loc-updater.key.pub)"
export TAURI_SIGNING_PRIVATE_KEY="$(cat ~/.tauri/loc-updater.key)"
export TAURI_UPDATER_ENDPOINT="https://github.com/codeflash-ai/locality/releases/latest/download/latest-macos.json"
make publish
```

The publish script copies the signed updater archive and signature to:

```text
target/release/bundle/updater/Locality-beta-YYYYMMDD-<commit>-macos-<arch>.app.tar.gz
target/release/bundle/updater/Locality-beta-YYYYMMDD-<commit>-macos-<arch>.app.tar.gz.sig
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
target/release/homebrew/Casks/loc.rb
```

Copy that file into the CodeFlash tap, for example:

```text
codeflash-ai/homebrew-tap/Casks/loc.rb
```

The generated cask declares `depends_on arch: :arm64`, so Intel Macs are not a
supported Homebrew installation target.

## GitHub Release Workflow

The GitHub workflow in `.github/workflows/release-macos.yml` publishes the
macOS channel from a `v*` tag or manual workflow dispatch. It runs on the
GitHub-hosted `macos-15` arm64 runner, builds the notarized DMG, produces the
signed updater archive, renders `latest-macos.json`, renders `loc.rb`, creates
or updates the GitHub Release, uploads the public DMG as
`Locality_Mac_v<version>.dmg`, also uploads the stable latest-download alias
`Locality_Mac.dmg`, and optionally pushes the cask to the Homebrew tap. The
separate `.github/workflows/release-notes.yml` workflow generates the GitHub
Release body with Codex from the commits since the previous `v*` tag. Platform
workflows create only a placeholder body when the release does not exist yet, so
asset publication can proceed before the notes workflow finishes.
Release creation is staged as prerelease and non-latest. The separate
`.github/workflows/release-finalize.yml` workflow promotes the release to latest
only after macOS, Linux, and Windows workflows have completed successfully and
all expected public download assets are present. Until then,
`/releases/latest/download/...` URLs continue to resolve to the previous complete
release.

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

The companion release-notes workflow requires `CODEX_CONFIG_TOML` plus the
provider credential it references. For the Azure OpenAI setup, that means
`AZURE_OPENAI_API_KEY`.

Optional repository secret:

- `APPLE_SIGNING_IDENTITY`: exact Developer ID identity if the imported
  keychain has more than one Developer ID Application certificate.
- `HOMEBREW_TAP_TOKEN`: fine-grained token with write access to the Homebrew tap
  repo. If omitted, the workflow still uploads `loc.rb` to the GitHub Release,
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

Mac App Store (MAS) distribution should be a separate build target from the
direct DMG and Homebrew release. The App Store build needs App Sandbox enabled,
App Store signing/provisioning instead of Developer ID signing, App Store
Connect metadata, and review of the File Provider extension, embedded sidecars,
and CLI install behavior. Keep the direct DMG/Homebrew build as the fast-moving
beta channel until the sandboxed build has its own install and update story.

Run the static readiness audit before attempting an App Store-signed build:

```sh
make audit-mas-readiness
```

The audit verifies that the checked-in macOS app/extension entitlements include
App Sandbox, the shared application group, and network client access; that the
bundle identifiers and minimum OS are consistent; and that the desktop frontend
and backend understand the `mas` distribution channel.

Build a local Mac App Store-channel `.app` bundle with:

```sh
make build-mas
```

This sets both build-time channel flags:

```sh
VITE_LOCALITY_DISTRIBUTION_CHANNEL=mas
LOCALITY_DISTRIBUTION_CHANNEL=mas
```

The `mas` channel disables the in-app Tauri updater and skips automatic
terminal-command symlink installation. The App Store build should get updates
through the App Store, while users who need `loc` on their shell path should use
the Homebrew or direct DMG channel.

Build a signed App Store package locally with:

```sh
make publish-mas
```

`make publish-mas` builds the `mas` channel `.app`, embeds provisioning profiles
for the containing app and File Provider extension, re-signs nested code with
the App Store application identity, and creates a signed `.pkg` with the App
Store installer identity. By default it only writes and validates the package
locally:

```text
target/release/bundle/mas/Locality-app-store-YYYYMMDD-<commit>-<arch>.pkg
target/release/bundle/mas/Locality-app-store-YYYYMMDD-<commit>-<arch>.pkg.sha256
```

Required local inputs:

- `MAS_APP_SIGNING_IDENTITY`: usually `3rd Party Mac Developer Application: ...`
  or `Apple Distribution: ...`; auto-detected when exactly one matching
  identity exists.
- `MAS_INSTALLER_SIGNING_IDENTITY`: `3rd Party Mac Developer Installer: ...`;
  auto-detected when exactly one matching identity exists.
- `MAS_APP_PROVISIONING_PROFILE` or `MAS_APP_PROVISIONING_PROFILE_BASE64`.
- `MAS_FILE_PROVIDER_PROVISIONING_PROFILE` or
  `MAS_FILE_PROVIDER_PROVISIONING_PROFILE_BASE64`.

Optional App Store Connect upload inputs:

- `APP_STORE_CONNECT_API_KEY_ID`.
- `APP_STORE_CONNECT_API_ISSUER_ID`.
- `APP_STORE_CONNECT_API_PRIVATE_KEY` or
  `APP_STORE_CONNECT_API_PRIVATE_KEY_PATH`.
- `MAS_VALIDATE_WITH_APPLE=1` to validate the `.pkg` with App Store Connect.
- `MAS_UPLOAD=1` to validate and upload the `.pkg` to App Store Connect.

The GitHub workflow in `.github/workflows/release-mas.yml` is manual-only. It
checks out an existing release tag, imports App Store application and installer
certificates, builds the signed package, and optionally validates/uploads it to
App Store Connect.

Required repository secrets:

- `MAS_APP_CERTIFICATE_P12_BASE64`.
- `MAS_APP_CERTIFICATE_PASSWORD`.
- `MAS_INSTALLER_CERTIFICATE_P12_BASE64`.
- `MAS_INSTALLER_CERTIFICATE_PASSWORD`.
- `MAS_APP_PROVISIONING_PROFILE_BASE64`.
- `MAS_FILE_PROVIDER_PROVISIONING_PROFILE_BASE64`.

Required for App Store Connect validation or upload:

- `APP_STORE_CONNECT_API_KEY_ID`.
- `APP_STORE_CONNECT_API_ISSUER_ID`.
- `APP_STORE_CONNECT_API_PRIVATE_KEY`.

Optional repository secrets:

- `MAS_APP_SIGNING_IDENTITY`.
- `MAS_INSTALLER_SIGNING_IDENTITY`.

Remaining App Store work:

- Create or confirm App Store App IDs for `ai.codeflash.locality` and
  `ai.codeflash.locality.Locality.FileProvider`.
- Create provisioning profiles for the containing app and File Provider
  extension with `C484HB7Q6S.group.ai.codeflash.locality`.
- Run the manual workflow with validation enabled.
- Run the manual workflow with upload enabled when validation passes.
- Complete TestFlight/App Review metadata in App Store Connect.

## Distribution Channels

Initial channel: notarized DMG direct download.

Power-user channel: Homebrew cask that installs the same notarized DMG.

Fast-moving channel: Tauri updater using the signed updater archive and
`latest-macos.json` manifest from the GitHub Release.

App Store channel: manual `release-mas` workflow after App Store provisioning
and metadata are ready.
