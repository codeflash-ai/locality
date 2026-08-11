#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAURI_CONF="${ROOT}/apps/desktop/src-tauri/tauri.conf.json"
PACKAGE_JSON="${ROOT}/apps/desktop/package.json"
MAKEFILE="${ROOT}/Makefile"
REPOSITORY_CONFIG="${ROOT}/scripts/linux-repository-config.sh"

fail() {
  printf 'linux publish config test: %s\n' "$*" >&2
  exit 1
}

require_executable() {
  local path="$1"
  [[ -x "${path}" ]] || fail "expected executable ${path}"
}

json_value() {
  jq -r "$1" "$2"
}

require_executable "${ROOT}/scripts/publish-linux.sh"
require_executable "${ROOT}/scripts/render-linux-repositories.sh"
[[ -f "${REPOSITORY_CONFIG}" ]] || fail "shared Linux repository configuration is missing"
require_executable "${ROOT}/apps/desktop/scripts/prepare-bundle.sh"
require_executable "${ROOT}/apps/desktop/scripts/prepare-linux-bundle.sh"
[[ -f "${ROOT}/apps/desktop/scripts/prepare-bundle.mjs" ]] \
  || fail "expected apps/desktop/scripts/prepare-bundle.mjs"

grep -q '^publish-linux:' "${MAKEFILE}" || fail "Makefile is missing publish-linux target"
grep -q '^render-linux-repositories:' "${MAKEFILE}" \
  || fail "Makefile is missing render-linux-repositories target"
! grep -q '^build-tauri-linux:' "${MAKEFILE}" \
  || fail "Makefile should expose one Linux publish target, publish-linux"
[[ "$(json_value '.scripts["build:linux"]' "${PACKAGE_JSON}")" == "tauri build --bundles deb,rpm" ]] \
  || fail "package.json build:linux must build deb and rpm bundles"

[[ "$(json_value '.build.beforeBundleCommand' "${TAURI_CONF}")" == "node ./scripts/prepare-bundle.mjs" ]] \
  || fail "Tauri beforeBundleCommand must dispatch per platform"

for binary in loc localityd locality-fuse; do
  [[ "$(json_value ".bundle.linux.deb.files[\"/usr/bin/${binary}\"]" "${TAURI_CONF}")" == "linux/${binary}" ]] \
    || fail "Debian package must install ${binary} into /usr/bin"
  [[ "$(json_value ".bundle.linux.rpm.files[\"/usr/bin/${binary}\"]" "${TAURI_CONF}")" == "linux/${binary}" ]] \
    || fail "RPM package must install ${binary} into /usr/bin"
done

[[ -f "${ROOT}/apps/desktop/src-tauri/icons/locality-mount-logo.png" ]] \
  || fail "Linux mount root logo PNG asset is missing"
[[ "$(json_value '.bundle.linux.deb.files["/usr/share/icons/hicolor/256x256/apps/locality-mount-logo.png"]' "${TAURI_CONF}")" == "icons/locality-mount-logo.png" ]] \
  || fail "Debian package must install the mount root logo icon theme asset"
[[ "$(json_value '.bundle.linux.rpm.files["/usr/share/icons/hicolor/256x256/apps/locality-mount-logo.png"]' "${TAURI_CONF}")" == "icons/locality-mount-logo.png" ]] \
  || fail "RPM package must install the mount root logo icon theme asset"

for dependency in fuse3 systemd; do
  json_value '.bundle.linux.deb.depends[]' "${TAURI_CONF}" | grep -qx "${dependency}" \
    || fail "Debian package must depend on ${dependency}"
  json_value '.bundle.linux.rpm.depends[]' "${TAURI_CONF}" | grep -qx "${dependency}" \
    || fail "RPM package must depend on ${dependency}"
done

grep -q 'appindicator3-0.1' "${ROOT}/scripts/publish-linux.sh" \
  || fail "publish-linux must prepare appindicator pkg-config metadata for Tauri"
grep -q 'PKG_CONFIG_PATH' "${ROOT}/scripts/publish-linux.sh" \
  || fail "publish-linux must export PKG_CONFIG_PATH when using temporary metadata"
grep -q 'copy_latest_alias' "${ROOT}/scripts/publish-linux.sh" \
  || fail "publish-linux must create stable latest-release artifact aliases"
grep -q 'appimage' "${ROOT}/scripts/publish-linux.sh" \
  || fail "publish-linux must build AppImage artifacts for Tauri self-update"
grep -q 'latest-linux.json' "${ROOT}/scripts/publish-linux.sh" \
  || fail "publish-linux must configure a Linux updater endpoint"
grep -q 'createrepo_c' "${ROOT}/scripts/render-linux-repositories.sh" \
  || fail "Linux repository renderer must create RPM metadata"
grep -q 'apt-ftparchive' "${ROOT}/scripts/render-linux-repositories.sh" \
  || fail "Linux repository renderer must create APT metadata"
grep -q 'LINUX_REPO_GPG_PRIVATE_KEY' "${ROOT}/scripts/render-linux-repositories.sh" \
  || fail "Linux repository renderer must support signed metadata"

for packaged_path in \
  /etc/apt/sources.list.d/locality.sources \
  /usr/share/keyrings/codeflash-locality-archive-keyring.gpg \
  /etc/yum.repos.d/locality.repo \
  /etc/zypp/repos.d/locality.repo \
  /etc/pki/rpm-gpg/RPM-GPG-KEY-codeflash-locality
do
  grep -F -q "${packaged_path}" "${ROOT}/scripts/publish-linux.sh" \
    || fail "release packages must include ${packaged_path}"
done

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-linux-repository-config.XXXXXX")"
cleanup() {
  rm -rf "${tmp_root}"
}
trap cleanup EXIT

(
  export LINUX_REPO_BASE_URL="https://packages.example.test/locality"
  source "${REPOSITORY_CONFIG}"
  write_apt_repository_source "${tmp_root}/locality.sources"
  write_rpm_repository_config \
    "${tmp_root}/locality.repo" \
    "file:///etc/pki/rpm-gpg/RPM-GPG-KEY-codeflash-locality" \
    1
)

cat > "${tmp_root}/expected.sources" <<'EOF'
Types: deb
URIs: https://packages.example.test/locality/apt
Suites: stable
Components: main
Architectures: amd64
Signed-By: /usr/share/keyrings/codeflash-locality-archive-keyring.gpg
EOF
diff -u "${tmp_root}/expected.sources" "${tmp_root}/locality.sources" \
  || fail "APT repository enrollment output changed"

cat > "${tmp_root}/expected.repo" <<'EOF'
[locality]
name=Locality
baseurl=https://packages.example.test/locality/rpm/$basearch
enabled=1
gpgcheck=0
repo_gpgcheck=1
gpgkey=file:///etc/pki/rpm-gpg/RPM-GPG-KEY-codeflash-locality
EOF
diff -u "${tmp_root}/expected.repo" "${tmp_root}/locality.repo" \
  || fail "RPM repository enrollment output changed"

config_json="$({
  export TAURI_UPDATER_PUBKEY="test-public-key"
  export TAURI_SIGNING_PRIVATE_KEY="test-private-key"
  export LINUX_REPO_GPG_PRIVATE_KEY="test-repository-key"
  source "${ROOT}/scripts/publish-linux.sh"
  build_config_json
})"
printf '%s' "${config_json}" | jq -e '
  .bundle.createUpdaterArtifacts == true and
  .bundle.linux.deb.files["/etc/apt/sources.list.d/locality.sources"] == "linux/repository/locality.sources" and
  .bundle.linux.rpm.files["/etc/yum.repos.d/locality.repo"] == "linux/repository/locality.repo" and
  .bundle.linux.rpm.files["/etc/zypp/repos.d/locality.repo"] == "linux/repository/locality.repo"
' >/dev/null || fail "Tauri release override must package repository enrollment files"
