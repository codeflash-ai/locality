#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAURI_CONF="${ROOT}/apps/desktop/src-tauri/tauri.conf.json"
PACKAGE_JSON="${ROOT}/apps/desktop/package.json"
MAKEFILE="${ROOT}/Makefile"

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
require_executable "${ROOT}/apps/desktop/scripts/prepare-bundle.sh"
require_executable "${ROOT}/apps/desktop/scripts/prepare-linux-bundle.sh"

grep -q '^publish-linux:' "${MAKEFILE}" || fail "Makefile is missing publish-linux target"
grep -q '^render-linux-repositories:' "${MAKEFILE}" \
  || fail "Makefile is missing render-linux-repositories target"
! grep -q '^build-tauri-linux:' "${MAKEFILE}" \
  || fail "Makefile should expose one Linux publish target, publish-linux"
[[ "$(json_value '.scripts["build:linux"]' "${PACKAGE_JSON}")" == "tauri build --bundles deb,rpm" ]] \
  || fail "package.json build:linux must build deb and rpm bundles"

[[ "$(json_value '.build.beforeBundleCommand' "${TAURI_CONF}")" == "./scripts/prepare-bundle.sh" ]] \
  || fail "Tauri beforeBundleCommand must dispatch per platform"

for binary in loc localityd locality-fuse; do
  [[ "$(json_value ".bundle.linux.deb.files[\"/usr/bin/${binary}\"]" "${TAURI_CONF}")" == "linux/${binary}" ]] \
    || fail "Debian package must install ${binary} into /usr/bin"
  [[ "$(json_value ".bundle.linux.rpm.files[\"/usr/bin/${binary}\"]" "${TAURI_CONF}")" == "linux/${binary}" ]] \
    || fail "RPM package must install ${binary} into /usr/bin"
done

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
