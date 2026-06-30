#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UNMOUNT_SCRIPT="${ROOT}/platform/macos/LocalityFileProvider/scripts/unmount-existing-domains.sh"
BUILD_SCRIPT="${ROOT}/platform/macos/LocalityFileProvider/scripts/build-dev-bundle.sh"
README="${ROOT}/platform/macos/LocalityFileProvider/README.md"
MACOS_DOC="${ROOT}/docs/macos-distribution.md"

fail() {
  printf 'macos file provider build config test: %s\n' "$*" >&2
  exit 1
}

[[ -x "${UNMOUNT_SCRIPT}" ]] \
  || fail "unmount-existing-domains.sh must be executable"
grep -F -q '"${ROOT}/scripts/unmount-existing-domains.sh"' "${BUILD_SCRIPT}" \
  || fail "build-dev-bundle must unmount existing File Provider domains before rebuilding"
grep -F -q 'reset --json' "${UNMOUNT_SCRIPT}" \
  || fail "unmount script must unregister File Provider domains with locality-file-providerctl reset"
grep -F -q 'LOCALITY_SKIP_FILE_PROVIDER_UNMOUNT_FOR_BUILD' "${UNMOUNT_SCRIPT}" \
  || fail "unmount script must expose an explicit build escape hatch"
grep -F -q 'LOCALITY_FILE_PROVIDERCTL' "${UNMOUNT_SCRIPT}" \
  || fail "unmount script must honor the development helper override"
grep -F -q '/Applications/Locality.app' "${UNMOUNT_SCRIPT}" \
  || fail "unmount script must check the system Applications install"
grep -F -q 'target/release/bundle/macos/Locality.app' "${UNMOUNT_SCRIPT}" \
  || fail "unmount script must check the previous Tauri bundle"
grep -F -q 'LOCALITY_SKIP_FILE_PROVIDER_UNMOUNT_FOR_BUILD=1' "${ROOT}/scripts/publish-macos.sh" \
  || fail "publish-macos must opt out of local File Provider unmounts"
grep -F -q 'LOCALITY_SKIP_FILE_PROVIDER_UNMOUNT_FOR_BUILD=1' "${ROOT}/scripts/publish-mas.sh" \
  || fail "publish-mas must opt out of local File Provider unmounts"
grep -F -q 'unregisters existing Locality File Provider domains' "${README}" \
  || fail "File Provider README must document rebuild unmount behavior"
grep -F -q 'unmounts Finder' "${MACOS_DOC}" \
  || fail "macOS distribution docs must document rebuild unmount behavior"

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-macos-fp-build-config.XXXXXX")"
cleanup() {
  rm -rf "${tmp_root}"
}
trap cleanup EXIT

fake_helper="${tmp_root}/locality-file-providerctl"
helper_log="${tmp_root}/helper.log"
cat >"${fake_helper}" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >"${LOCALITY_FAKE_FILE_PROVIDERCTL_LOG}"
printf '{"ok":true,"action":"reset"}\n'
EOF
chmod +x "${fake_helper}"

LOCALITY_FILE_PROVIDERCTL="${fake_helper}" \
  LOCALITY_FAKE_FILE_PROVIDERCTL_LOG="${helper_log}" \
  "${UNMOUNT_SCRIPT}" >/dev/null

[[ "$(cat "${helper_log}")" == "reset --json" ]] \
  || fail "unmount script must invoke the helper as: reset --json"
