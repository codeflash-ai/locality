#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAKEFILE="${ROOT}/Makefile"
PUBLISH_SCRIPT="${ROOT}/scripts/publish-macos.sh"
HOMEBREW_SCRIPT="${ROOT}/scripts/render-homebrew-cask.sh"

fail() {
  printf 'macos publish config test: %s\n' "$*" >&2
  exit 1
}

grep -q '^publish: setup' "${MAKEFILE}" \
  || fail "Makefile is missing publish target"
grep -q '^publish-unnotarized: setup' "${MAKEFILE}" \
  || fail "Makefile is missing publish-unnotarized target"
grep -q 'PUBLISH_SKIP_NOTARIZATION=1 scripts/publish-macos.sh' "${MAKEFILE}" \
  || fail "publish-unnotarized must run publish-macos with PUBLISH_SKIP_NOTARIZATION=1"
grep -q '^test-macos-publish-config:' "${MAKEFILE}" \
  || fail "Makefile is missing test-macos-publish-config target"

grep -q 'skip_notarization()' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must define skip_notarization"
grep -q 'optional_signing_identity()' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must allow unnotarized builds without Developer ID signing"
grep -F -q 'signing_identity="$(optional_signing_identity)"' "${PUBLISH_SCRIPT}" \
  || fail "unnotarized publish must use optional signing identity detection"
grep -q 'require_developer_id="0"' "${PUBLISH_SCRIPT}" \
  || fail "unnotarized publish must not require Developer ID signatures"
grep -q 'require_developer_id="1"' "${PUBLISH_SCRIPT}" \
  || fail "notarized publish must still require Developer ID signatures"
grep -q 'PUBLISH_SKIP_NOTARIZATION' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must read PUBLISH_SKIP_NOTARIZATION"
grep -q 'LOCALITY_SKIP_FILE_PROVIDER_UNMOUNT_FOR_BUILD=1' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must not unregister local File Provider domains while publishing"
grep -q 'notary_args' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must keep notarized publish support"
grep -q 'skipping notarization and stapling' "${PUBLISH_SCRIPT}" \
  || fail "publish-macos must skip notarization and stapling in unnotarized mode"
grep -q 'dmg_status="unnotarized"' "${PUBLISH_SCRIPT}" \
  || fail "unnotarized artifacts must be named distinctly"
grep -q 'dmg_status="notarized"' "${PUBLISH_SCRIPT}" \
  || fail "notarized artifacts must keep the notarized naming suffix"

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-macos-publish-config.XXXXXX")"
cleanup() {
  rm -rf "${tmp_root}"
}
trap cleanup EXIT

dmg_dir="${tmp_root}/dmg"
cask_output="${tmp_root}/loc.rb"
mkdir -p "${dmg_dir}"
printf 'old notarized dmg\n' >"${dmg_dir}/Locality-release-20260619-abcdefg-notarized-aarch64.dmg"
printf 'new unnotarized dmg\n' >"${dmg_dir}/Locality-release-20260620-abcdefg-unnotarized-aarch64.dmg"

HOMEBREW_DMG_DIR="${dmg_dir}" \
  HOMEBREW_CASK_OUTPUT="${cask_output}" \
  HOMEBREW_RELEASE_TAG="v0.1.0" \
  HOMEBREW_VERSION="0.1.0" \
  "${HOMEBREW_SCRIPT}" >/dev/null

grep -q 'Locality-release-20260619-abcdefg-notarized-aarch64.dmg' "${cask_output}" \
  || fail "Homebrew cask should auto-select notarized DMGs"
if grep -q 'Locality-release-20260620-abcdefg-unnotarized-aarch64.dmg' "${cask_output}"; then
  fail "Homebrew cask must not auto-select unnotarized DMGs"
fi
