# Linux Distribution

AFS ships on Linux as Tauri-generated `.deb` and `.rpm` packages. The Linux
packages do not need signing, notarization, or stapling, but they do need the
same runtime sidecars that the macOS app bundle carries: the `afs` CLI, the
`afsd` daemon, and the `afs-fuse` projection helper.

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
which builds `afs`, `afsd`, and `afs-fuse` in release mode and stages them under
`apps/desktop/src-tauri/linux/`. Tauri includes those staged binaries in both
Linux package formats at:

```text
/usr/bin/afs
/usr/bin/afsd
/usr/bin/afs-fuse
```

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
target/release/bundle/linux/AFS-beta-YYYYMMDD-<commit>-<arch>.deb
target/release/bundle/linux/AFS-beta-YYYYMMDD-<commit>-<arch>.deb.sha256
target/release/bundle/linux/AFS-beta-YYYYMMDD-<commit>-<arch>.rpm
target/release/bundle/linux/AFS-beta-YYYYMMDD-<commit>-<arch>.rpm.sha256
target/release/bundle/linux/AFS-beta-linux-<arch>.deb
target/release/bundle/linux/AFS-beta-linux-<arch>.deb.sha256
target/release/bundle/linux/AFS-beta-linux-<arch>.rpm
target/release/bundle/linux/AFS-beta-linux-<arch>.rpm.sha256
```

Useful overrides:

```sh
PUBLISH_CHANNEL=release make publish-linux
PUBLISH_DATE=20260617 make publish-linux
```

Release builds with `TAURI_UPDATER_PUBKEY` and `TAURI_SIGNING_PRIVATE_KEY`
also produce a signed AppImage updater artifact:

```text
target/release/bundle/updater/AFS-release-YYYYMMDD-<commit>-linux-<arch>.AppImage
target/release/bundle/updater/AFS-release-YYYYMMDD-<commit>-linux-<arch>.AppImage.sig
target/release/bundle/updater/AFS-release-linux-<arch>.AppImage
target/release/bundle/updater/AFS-release-linux-<arch>.AppImage.sig
```

## Runtime Requirements

The package metadata declares `fuse3` and `systemd` dependencies. AFS needs
`fusermount3` and `/dev/fuse` for Linux FUSE mounts, and it uses `systemctl
--user` to manage one per-mount FUSE service.

The desktop tray requires either `libayatana-appindicator3` or
`libappindicator3`. Tauri detects that library through pkg-config during
bundling. When a distro provides the runtime library but omits the pkg-config
metadata from the installed package set, `scripts/publish-linux.sh` creates
temporary pkg-config metadata from `ldconfig` so the package build can continue.

Linux package validation checks that both packages contain:

```text
/usr/bin/afs
/usr/bin/afsd
/usr/bin/afs-fuse
```

The existing FUSE smoke test remains the runtime check for actual mount
behavior:

```sh
AFS_FUSE_SMOKE=1 AFS_FUSE_SMOKE_REQUIRED=1 make test-linux-fuse
```

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
curl -L -o /tmp/afs.deb https://github.com/codeflash-ai/afs/releases/latest/download/AFS-release-linux-x86_64.deb && sudo apt install /tmp/afs.deb
```

The workflow still renders versioned package files inside the APT/RPM
repositories deployed to GitHub Pages, but it does not upload those duplicate
versioned files to the GitHub Release page.

The same workflow renders static APT and RPM repository metadata under
`target/release/linux-repo` and deploys it to GitHub Pages for non-prerelease
builds. The default repository base URL is:

```text
https://codeflash-ai.github.io/afs
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

Repository publishing also requires GitHub Pages configured to deploy from
GitHub Actions.

## APT Repository

APT metadata is generated with `dpkg-scanpackages` and `apt-ftparchive`:

```text
apt/dists/stable/Release
apt/dists/stable/InRelease
apt/dists/stable/main/binary-amd64/Packages
apt/dists/stable/main/binary-amd64/Packages.gz
apt/pool/main/a/afs/*.deb
```

User install command:

```sh
curl -fsSL https://codeflash-ai.github.io/afs/apt/codeflash-afs.asc | sudo gpg --dearmor -o /usr/share/keyrings/codeflash-afs.gpg && echo "deb [signed-by=/usr/share/keyrings/codeflash-afs.gpg] https://codeflash-ai.github.io/afs/apt stable main" | sudo tee /etc/apt/sources.list.d/afs.list >/dev/null && sudo apt update && sudo apt install afs
```

Updates then use the normal distro path:

```sh
sudo apt update && sudo apt upgrade afs
```

## RPM/DNF Repository

RPM metadata is generated with `createrepo_c`:

```text
rpm/x86_64/repodata/repomd.xml
rpm/x86_64/*.rpm
rpm/afs.repo
```

When `LINUX_REPO_GPG_PRIVATE_KEY` is set, the workflow signs `repomd.xml` and
writes the public key to `rpm/RPM-GPG-KEY-codeflash-afs`. The generated
`afs.repo` enables `repo_gpgcheck=1` in that case. RPM package payload signing
is separate and not enabled yet, so `gpgcheck=0` remains in the generated repo
file until package signing is added.

User install command:

```sh
sudo curl -fsSL -o /etc/yum.repos.d/afs.repo https://codeflash-ai.github.io/afs/rpm/afs.repo && sudo dnf install afs
```

Updates then use:

```sh
sudo dnf upgrade afs
```

## Linux Tauri Self-Update

Tauri self-update on Linux is the AppImage channel. The Linux release workflow
builds `deb,rpm,appimage` when updater signing secrets are present, copies the
signed AppImage into release assets, and renders:

```text
target/release/bundle/updater/latest-linux.json
```

Linux packages installed through APT/DNF should update through APT/DNF. Users
who want Tauri-managed self-update should run the AppImage channel instead.

AppImage install command:

```sh
mkdir -p ~/.local/bin && curl -L -o ~/.local/bin/AFS.AppImage https://github.com/codeflash-ai/afs/releases/latest/download/AFS-release-linux-x86_64.AppImage && chmod +x ~/.local/bin/AFS.AppImage
```

The workflow shares the same release concurrency group as the macOS workflow so
both jobs can target one tag without racing while creating or updating the
GitHub Release.

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
