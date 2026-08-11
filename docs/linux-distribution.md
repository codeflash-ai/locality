# Linux Distribution

Locality ships on Linux as Tauri-generated `.deb` and `.rpm` packages. The Linux
packages do not need signing, notarization, or stapling, but they do need the
same runtime sidecars that the macOS app bundle carries: the `loc` CLI, the
`localityd` daemon, and the `locality-fuse` projection helper.

## Local Package Build

Build, validate, rename, and checksum both Linux artifacts:

```sh
make publish-linux
```

The Tauri pre-bundle hook runs:

```sh
apps/desktop/scripts/prepare-bundle.sh
```

On Linux that dispatches to `apps/desktop/scripts/prepare-linux-bundle.sh`,
which builds `loc`, `localityd`, and `locality-fuse` in release mode and stages them under
`apps/desktop/src-tauri/linux/`. Tauri includes those staged binaries in both
Linux package formats at:

```text
/usr/bin/loc
/usr/bin/localityd
/usr/bin/locality-fuse
/usr/share/icons/hicolor/256x256/apps/locality-mount-logo.png
```

The icon theme asset backs the shared FUSE root `.directory` metadata file. It
does not create files inside projected workspace directories.

Expected local artifacts:

```text
target/release/bundle/deb/*.deb
target/release/bundle/rpm/*.rpm
```

The publish script requires a clean git working tree by default because the
published filename includes the `HEAD` commit. Use `PUBLISH_ALLOW_DIRTY=1` only
for local throwaway builds.

Final artifacts are copied to:

```text
target/release/bundle/linux/Locality-beta-YYYYMMDD-<commit>-<arch>.deb
target/release/bundle/linux/Locality-beta-YYYYMMDD-<commit>-<arch>.deb.sha256
target/release/bundle/linux/Locality-beta-YYYYMMDD-<commit>-<arch>.rpm
target/release/bundle/linux/Locality-beta-YYYYMMDD-<commit>-<arch>.rpm.sha256
target/release/bundle/linux/Locality-beta-linux-<arch>.deb
target/release/bundle/linux/Locality-beta-linux-<arch>.deb.sha256
target/release/bundle/linux/Locality-beta-linux-<arch>.rpm
target/release/bundle/linux/Locality-beta-linux-<arch>.rpm.sha256
```

Useful overrides:

```sh
PUBLISH_CHANNEL=release make publish-linux
PUBLISH_DATE=20260617 make publish-linux
```

Release builds with `TAURI_UPDATER_PUBKEY` and `TAURI_SIGNING_PRIVATE_KEY`
also produce a signed AppImage updater artifact:

```text
target/release/bundle/updater/Locality-release-YYYYMMDD-<commit>-linux-<arch>.AppImage
target/release/bundle/updater/Locality-release-YYYYMMDD-<commit>-linux-<arch>.AppImage.sig
target/release/bundle/updater/Locality-release-linux-<arch>.AppImage
target/release/bundle/updater/Locality-release-linux-<arch>.AppImage.sig
```

## Runtime Requirements

The package metadata declares `fuse3` and `systemd` dependencies. Locality needs
`fusermount3` and `/dev/fuse` for Linux FUSE mounts, and it uses `systemctl
--user` to manage one shared-root FUSE service per Locality root.

The desktop tray requires either `libayatana-appindicator3` or
`libappindicator3`. Tauri detects that library through pkg-config during
bundling. When a distro provides the runtime library but omits the pkg-config
metadata from the installed package set, `scripts/publish-linux.sh` creates
temporary pkg-config metadata from `ldconfig` so the package build can continue.

Linux package validation checks that both packages contain:

```text
/usr/bin/loc
/usr/bin/localityd
/usr/bin/locality-fuse
```

Stable release packages also enroll the signed package repository. Debian
packages carry `/etc/apt/sources.list.d/locality.sources` and a dedicated keyring.
RPM packages carry `locality.repo` for both DNF/YUM and zypper plus the repository
public key. Beta/local builds omit enrollment unless
`LINUX_REPO_GPG_PRIVATE_KEY` is provided.

The existing FUSE smoke test remains the runtime check for actual mount
behavior:

```sh
LOCALITY_FUSE_SMOKE=1 LOCALITY_FUSE_SMOKE_REQUIRED=1 make test-linux-fuse
```

## Uninstall Cleanup

The Debian and RPM packages install a pre-remove hook. On actual package
removal, not package upgrade, the hook runs the desktop binary's
`--prepare-uninstall` cleanup entry point when available and then runs
`loc daemon stop` as a fallback. The cleanup stops Locality runtime processes
owned by the user, clears Locality local state and credentials, and removes
Locality-managed agent guidance and MCP `loc` entries. User-visible mount
folders are left in place.

## GitHub Release Workflow

The GitHub workflow in `.github/workflows/release-linux.yml` publishes Linux
packages from a `v*` tag or manual workflow dispatch. It runs on
`ubuntu-24.04`, installs the GTK/WebKit/FUSE/AppIndicator packaging
dependencies, runs `make publish-linux`, and uploads the resulting `.deb`,
`.rpm`, signed AppImage updater artifact, updater manifest, and
`SHA256SUMS-linux` to the matching GitHub Release.

GitHub Release uploads use stable asset names so latest-release install URLs do
not need to know the version or commit:

```sh
curl -L -o /tmp/loc.deb https://github.com/codeflash-ai/locality/releases/latest/download/Locality_Linux.deb && sudo apt install /tmp/loc.deb
```

The GitHub Release also includes the versioned public aliases
`Locality_Linux_v<version>.deb`, `Locality_Linux_v<version>.rpm`, and
`Locality_Linux_v<version>.AppImage` for users who want the exact release asset.
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

For stable releases, the Linux workflow uploads the date-and-commit packages as
a short-lived workflow artifact. The separate
`.github/workflows/publish-linux-repositories.yml` workflow runs from `main`,
renders signed APT and RPM metadata, rebuilds the existing Jekyll documentation
site, switches Pages from its legacy branch build to Actions deployments,
overlays the repositories, and deploys the combined site to GitHub Pages.
Running the Pages deployment from `main` satisfies the protected `github-pages`
environment without allowing release tags to deploy arbitrary site content.
Changes under `docs/` also run the same Pages workflow; those runs rebuild the
site and regenerate repository metadata from the newest release carrying the
repository marker (or the current latest release before the first marker), so
documentation publishing cannot erase or roll back the package repositories.

After deployment, the workflow uploads `linux-repository.json` to the GitHub
Release and retries release finalization. The finalizer requires that marker, so
a stable release cannot become latest before package-manager updates are live.
The repository workflow can also be dispatched with an existing stable tag to
rebuild from that release's versioned DEB and RPM assets during recovery.
The default repository base URL is:

```text
https://codeflash-ai.github.io/locality
```

Set the optional `LINUX_REPO_BASE_URL` repository variable if the package
repository is hosted somewhere else.

Required repository secrets:

- `TAURI_UPDATER_PUBKEY`: public updater signing key.
- `TAURI_SIGNING_PRIVATE_KEY`: private updater signing key.
- `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: updater key password, if one was set.
- `LINUX_REPO_GPG_PRIVATE_KEY`: ASCII-armored GPG private key used to sign APT
  and RPM repository metadata.
- `LINUX_REPO_GPG_PASSPHRASE`: passphrase for that key, if any.

The companion release-notes workflow requires `CODEX_CONFIG_TOML` plus the
provider credential it references. For the Azure OpenAI setup, that means
`AZURE_OPENAI_API_KEY`.

Repository publishing uses the existing GitHub Pages site and preserves the
rendered engineering documentation alongside `/apt` and `/rpm`.

## APT Repository

APT metadata is generated with `dpkg-scanpackages` and `apt-ftparchive`:

```text
apt/dists/stable/Release
apt/dists/stable/InRelease
apt/dists/stable/main/binary-amd64/Packages
apt/dists/stable/main/binary-amd64/Packages.gz
apt/pool/main/l/locality/*.deb
apt/locality.sources
```

User install command:

```sh
curl -fsSL https://codeflash-ai.github.io/locality/apt/codeflash-locality.asc | sudo gpg --dearmor -o /usr/share/keyrings/codeflash-locality-archive-keyring.gpg
sudo curl -fsSL -o /etc/apt/sources.list.d/locality.sources https://codeflash-ai.github.io/locality/apt/locality.sources
sudo apt update && sudo apt install locality
```

Updates then use the normal distro path:

```sh
sudo apt update && sudo apt upgrade
```

## RPM/DNF Repository

RPM metadata is generated with `createrepo_c`:

```text
rpm/x86_64/repodata/repomd.xml
rpm/x86_64/*.rpm
rpm/locality.repo
```

When `LINUX_REPO_GPG_PRIVATE_KEY` is set, the workflow signs `repomd.xml` and
writes the public key to `rpm/RPM-GPG-KEY-codeflash-locality`. The generated
`locality.repo` enables `repo_gpgcheck=1` in that case. RPM package payload signing
is separate and not enabled yet, so `gpgcheck=0` remains in the generated repo
file until package signing is added.

User install command:

```sh
sudo curl -fsSL -o /etc/yum.repos.d/locality.repo https://codeflash-ai.github.io/locality/rpm/locality.repo
sudo dnf install locality
```

Updates then use:

```sh
sudo dnf upgrade
```

The same RPM-MD repository supports `yum update` and openSUSE Tumbleweed. The
RPM package writes the same definition to `/etc/zypp/repos.d/locality.repo`, so
`sudo zypper refresh && sudo zypper update` discovers new Locality versions.

## Linux Tauri Self-Update

Tauri self-update on Linux is the AppImage channel. The Linux release workflow
builds `deb,rpm,appimage` when updater signing secrets are present, copies the
signed AppImage into release assets, and renders:

```text
target/release/bundle/updater/latest-linux.json
```

Linux packages installed through APT, DNF/YUM, or zypper update through that
package manager. The package identity is `locality`; `loc` is the CLI binary.
Users who want Tauri-managed self-update should run the AppImage channel
instead.

AppImage install command:

```sh
mkdir -p ~/.local/bin && curl -L -o ~/.local/bin/Locality.AppImage https://github.com/codeflash-ai/locality/releases/latest/download/Locality_Linux.AppImage && chmod +x ~/.local/bin/Locality.AppImage
```

The Linux workflow can publish assets before the release-notes workflow
finishes. In that case the GitHub Release body starts as a short placeholder and
is replaced when `.github/workflows/release-notes.yml` completes.

Release a new Linux package by updating the app version, committing the change,
tagging that commit, and pushing the tag:

```sh
git tag v0.1.1
git push origin v0.1.1
```

The workflow requires the tag to match `apps/desktop/src-tauri/tauri.conf.json`
exactly. For example, version `0.1.1` must be released as `v0.1.1`.

APT and DNF repositories are the primary Linux distribution channels. Snap and
Flatpak should be evaluated separately after the packaged FUSE and per-user
systemd behavior has been tested on the target distribution.
