# Windows Distribution

Locality ships on Windows as a Tauri-generated NSIS installer. The Windows package
contains the desktop app plus the sidecars needed for the product runtime:
`loc.exe`, `localityd.exe`, and `locality-cloud-files.exe`.

## Local Package Build

Build a local NSIS installer from the repo root on Windows:

```powershell
make build-tauri-windows
```

For release-like local packaging, use:

```powershell
pwsh -NoProfile -ExecutionPolicy Bypass -File scripts/publish-windows.ps1
```

The Tauri pre-bundle hook runs:

```powershell
apps/desktop/scripts/prepare-windows-bundle.ps1
```

That script builds `loc-cli`, `localityd`, and `locality-cloud-files` in release mode,
stages the three sidecars under `apps/desktop/src-tauri/windows/`, and lets the
NSIS hook copy them next to the installed desktop executable.

Expected local artifacts:

```text
target/release/bundle/nsis/*.exe
target/release/bundle/windows/Locality-beta-windows-x86_64-setup.exe
target/release/bundle/windows/Locality-beta-windows-x86_64-setup.exe.sha256
```

The publish script requires a clean git working tree by default because the
published filename includes the `HEAD` commit. Use `PUBLISH_ALLOW_DIRTY=1` only
for local throwaway builds.

## Runtime Behavior

The Windows desktop app uses the same platform lifecycle layer as the CLI. When
a Windows Cloud Files mount is activated or opened, the app registers the sync
root if needed and supervises `locality-cloud-files.exe run` for that mount.
During first activation, the desktop app waits for `localityd` to expose
mount-point children before starting the Cloud Files runtime, so the helper seeds
root placeholders from discovered workspace metadata instead of from an empty
initial projection.

Installed files:

```text
Locality.exe
loc.exe
localityd.exe
locality-cloud-files.exe
```

The NSIS uninstall hook stops the desktop app, `localityd.exe`, `loc.exe`, and
Cloud Files provider runtimes; runs the desktop binary's `--prepare-uninstall`
cleanup entry point; removes installed sidecars; removes the per-user Windows
login item; removes Locality-managed terminal command shims; and removes
Locality-managed agent guidance and MCP `loc` entries. It does not delete
user-visible mount folders.

For destructive local test-machine cleanup from a source checkout, use:

```powershell
make clean-start-plan
make clean-start
```

On Windows, `make clean-start-plan` runs `scripts/clean-start.ps1` without
mutating state. `make clean-start` passes `-Yes`; the script stops Locality
desktop, CLI, daemon, and Cloud Files helper processes, runs
`Locality.exe --prepare-uninstall` or `locality-desktop.exe --prepare-uninstall`
from the install directory when available, removes the per-user login item and
Locality-managed `loc.cmd` shims, resets Locality Windows Cloud Files
registrations, and deletes the Locality install/state directory plus the default
user-visible mount folder at `%USERPROFILE%\Locality`. If stale Cloud Files
placeholders survive provider teardown and normal deletion fails, the script
temporarily detaches `CldFlt` from the mount volume, removes the mount folder,
and reattaches the filter before continuing. That fallback is destructive,
machine-local, and may require an elevated shell. By default the install and
state directory is `%LOCALAPPDATA%\Locality`; pass `-StateDir` or `-InstallDir`
when validating a non-default install layout. Pass `-KeepCredentials` for
install-state testing that should not invoke the credential-clearing uninstall
hook.

Because the current-user install directory is also the default Locality state
root, **Reset Local State** removes metadata, caches, provider state, logs, and
SQLite state from this folder while preserving the installed desktop executable,
sidecars, uninstaller, and terminal command shim directory. On Windows the
desktop app pauses Cloud Files supervision and stops any persisted
`locality-cloud-files.exe` runtimes recorded under `cloud-files-lifecycle/`
before clearing `state.sqlite3`, so the reset path does not race a still-running
provider against SQLite deletion.

## Code Signing

Release builds can Authenticode-sign the sidecars before NSIS packaging and sign
the final installer after packaging. The GitHub release workflow uses Azure
Artifact Signing with OpenID Connect when the Azure signing configuration is
present, so the Windows signing key stays in Azure instead of being exported as a
PFX into GitHub. If none of the Azure signing values are configured, the workflow
publishes unsigned Windows installer assets and emits a warning. A partial Azure
configuration fails the release because that usually means signing was intended
but misconfigured.

Optional repository secrets for Authenticode signing:

- `AZURE_CLIENT_ID`: client/application ID for the Entra app registration used
  by GitHub Actions OIDC.
- `AZURE_TENANT_ID`: Azure tenant/directory ID.
- `AZURE_SUBSCRIPTION_ID`: Azure subscription ID containing the Artifact
  Signing account.
- `AZURE_ARTIFACT_SIGNING_ENDPOINT`: region endpoint for the Artifact Signing
  account, for example `https://eus.codesigning.azure.net/`.
- `AZURE_ARTIFACT_SIGNING_ACCOUNT`: Artifact Signing account name.
- `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE`: certificate profile name.

The Entra app/service principal must have a federated credential for this
repository and the `Artifact Signing Certificate Profile Signer` role on the
Artifact Signing account, resource group, or subscription.

For local release-like signing with an exportable Authenticode certificate, set:

```powershell
$env:LOCALITY_WINDOWS_CODESIGN = "1"
$env:WINDOWS_CODESIGN_CERT_SHA1 = "<certificate-thumbprint>"
```

Optional:

```powershell
$env:WINDOWS_SIGNTOOL = "C:\Path\To\signtool.exe"
$env:WINDOWS_CODESIGN_TIMESTAMP_URL = "http://timestamp.digicert.com"
```

`WINDOWS_CODESIGN_CERT_SUBJECT` or `WINDOWS_CODESIGN_SUBJECT` can be used
instead of `WINDOWS_CODESIGN_CERT_SHA1` for local signing when the certificate
subject is unique.

Set `PUBLISH_REQUIRE_SIGNING=1` to fail the release build if no signing
provider is available.

## Updater Artifacts

When `TAURI_UPDATER_PUBKEY` and `TAURI_SIGNING_PRIVATE_KEY` are set,
`scripts/publish-windows.ps1` asks Tauri to create signed updater artifacts and
copies the stable alias to:

```text
target/release/bundle/windows/Locality-release-windows-x86_64-setup.exe
target/release/bundle/windows/Locality-release-windows-x86_64-setup.exe.sig
```

When the GitHub workflow Authenticode-signs the installer through Azure Artifact
Signing, it regenerates the Tauri updater `.sig` after Authenticode signing so
the updater signature covers the final published bytes.

Render the Windows updater manifest with:

```powershell
$env:UPDATER_MANIFEST_OUTPUT = "target/release/bundle/updater/latest-windows.json"
$env:UPDATER_WINDOWS_X86_64_ARTIFACT = "target/release/bundle/windows/Locality-release-windows-x86_64-setup.exe"
bash scripts/render-tauri-updater-manifest.sh
```

## GitHub Release Workflow

The GitHub workflow in `.github/workflows/release-windows.yml` publishes the
Windows channel from a `v*` tag or manual workflow dispatch. It runs on
`windows-latest`, verifies the tag points at current `main`, verifies the tag
matches the Tauri app version, signs sidecars and installers through Azure
Artifact Signing when configured, builds the NSIS package, renders
`latest-windows.json`, creates or updates the GitHub Release, and uploads:

```text
Locality_Windows_v0.1.0.exe
Locality_Windows_v0.1.0.exe.sha256
Locality_Windows_v0.1.0.exe.sig
Locality_Windows.exe
Locality_Windows.exe.sha256
Locality_Windows.exe.sig
latest-windows.json
SHA256SUMS-windows
```

The separate `.github/workflows/release-notes.yml` workflow generates the
GitHub Release body with Codex from the commits since the previous reachable
`v*` tag. Platform workflows create only a placeholder body when the release
does not exist yet.
Release creation is staged as prerelease and non-latest. The separate
`.github/workflows/release-finalize.yml` workflow promotes the release to latest
only after macOS, Linux, and Windows workflows have completed successfully and
all expected public download assets are present. Until then,
`/releases/latest/download/...` URLs continue to resolve to the previous complete
release.

Azure Artifact Signing uses GitHub OIDC. The Azure federated credentials must
allow the tag-triggered workflow subject, preferably for all release tags such
as `repo:codeflash-ai/locality:ref:refs/tags/v*`. Without that tag subject, the
signing login fails before packaging starts.

Required repository secrets:

- `TAURI_UPDATER_PUBKEY`: public updater signing key.
- `TAURI_SIGNING_PRIVATE_KEY`: private updater signing key.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: updater key password, if one was set.

The companion release-notes workflow requires `CODEX_CONFIG_TOML` plus the
provider credential it references. For the Azure OpenAI setup, that means
`AZURE_OPENAI_API_KEY`.

Optional repository secrets for Authenticode signing:

- `AZURE_CLIENT_ID`: client/application ID for the Entra app registration used
  by GitHub Actions OIDC.
- `AZURE_TENANT_ID`: Azure tenant/directory ID.
- `AZURE_SUBSCRIPTION_ID`: Azure subscription ID containing the Artifact
  Signing account.
- `AZURE_ARTIFACT_SIGNING_ENDPOINT`: region endpoint for the Artifact Signing
  account, for example `https://eus.codesigning.azure.net/`.
- `AZURE_ARTIFACT_SIGNING_ACCOUNT`: Artifact Signing account name.
- `AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE`: certificate profile name.

Optional repository variable:

- `WINDOWS_CODESIGN_TIMESTAMP_URL`: timestamp server URL. Defaults to
  `http://timestamp.digicert.com`.

## Live E2E

The Windows live Notion test is `tests/windows_cloud_files_live.ps1`. It creates
a disposable Notion page, mounts it as `windows-cloud-files`, starts `localityd`,
starts the Cloud Files provider, runs `loc doctor --json` against the live
state, then verifies browse, hydrate, edit, push, create, rename, delete, and
archive behavior through the real mounted directory.

Run locally:

```powershell
$env:NOTION_TOKEN = "..."
$env:LOCALITY_NOTION_LIVE_PARENT_PAGE = "..."
$env:LOCALITY_WINDOWS_CLOUD_FILES_LIVE = "1"
pwsh ./tests/windows_cloud_files_live.ps1
```

GitHub Actions runs the same script from
`.github/workflows/notion-live-e2e.yml` on `windows-latest` when the
`notion-live-e2e` environment provides `NOTION_TOKEN` and
`LOCALITY_NOTION_LIVE_PARENT_PAGE`.
