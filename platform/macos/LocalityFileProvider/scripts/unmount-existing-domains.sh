#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "${ROOT}/../../.." && pwd)"
DEFAULT_DEST="${HOME}/Applications/Locality.app"
DEST="${LOCALITY_APP_DEST:-${DEFAULT_DEST}}"
SYSTEM_DEST="/Applications/Locality.app"
TAURI_BUNDLE_DEST="${REPO_ROOT}/target/release/bundle/macos/Locality.app"

if [[ "${LOCALITY_SKIP_FILE_PROVIDER_UNMOUNT_FOR_BUILD:-}" == "1" ]]; then
  echo "Skipping File Provider unmount because LOCALITY_SKIP_FILE_PROVIDER_UNMOUNT_FOR_BUILD=1" >&2
  exit 0
fi

required_helpers=()
if [[ -n "${LOCALITY_FILE_PROVIDERCTL:-}" ]]; then
  required_helpers+=("${LOCALITY_FILE_PROVIDERCTL}")
fi
required_helpers+=(
  "${DEST}/Contents/MacOS/locality-file-providerctl"
  "${DEFAULT_DEST}/Contents/MacOS/locality-file-providerctl"
  "${SYSTEM_DEST}/Contents/MacOS/locality-file-providerctl"
  "${TAURI_BUNDLE_DEST}/Contents/MacOS/locality-file-providerctl"
)
fallback_helpers=(
  "${ROOT}/.build/dev-bundle/Locality.app/Contents/MacOS/locality-file-providerctl"
  "${REPO_ROOT}/apps/desktop/src-tauri/macos/LocalityFileProvider/locality-file-providerctl"
)

seen_helpers=""
found_required_helper="0"
found_fallback_helper="0"
last_error=""

try_helper() {
  local helper="$1"
  local required="$2"
  local output

  [[ -n "${helper}" ]] || return 0
  case ":${seen_helpers}:" in
    *":${helper}:"*) return 0 ;;
  esac
  seen_helpers="${seen_helpers}:${helper}"

  [[ -x "${helper}" ]] || return 0
  if [[ "${required}" == "1" ]]; then
    found_required_helper="1"
  else
    found_fallback_helper="1"
  fi

  echo "Unmounting existing Locality File Provider domains with ${helper}" >&2
  if output="$("${helper}" reset --json 2>&1)"; then
    echo "Unmounted existing Locality File Provider domains" >&2
    exit 0
  fi
  last_error="${output}"
  printf 'File Provider unmount failed with %s:\n%s\n' "${helper}" "${output}" >&2
}

for helper in "${required_helpers[@]}"; do
  try_helper "${helper}" "1"
done

for helper in "${fallback_helpers[@]}"; do
  try_helper "${helper}" "0"
done

if [[ "${found_required_helper}" == "0" && "${found_fallback_helper}" == "0" ]]; then
  echo "No existing locality-file-providerctl found; skipping File Provider unmount" >&2
  exit 0
fi

if [[ "${found_required_helper}" == "0" ]]; then
  printf 'Only stale build-output locality-file-providerctl helpers were found, and none could reset File Provider domains; continuing rebuild.\n' >&2
  exit 0
fi

printf 'Unable to unmount existing Locality File Provider domains before rebuild.\n' >&2
if [[ -n "${last_error}" ]]; then
  printf 'Last error:\n%s\n' "${last_error}" >&2
fi
exit 1
