#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="$("${ROOT}/scripts/build-dev-bundle.sh")"
DEFAULT_DEST="${HOME}/Applications/Locality.app"
DEST="${LOCALITY_APP_DEST:-${DEFAULT_DEST}}"
LEGACY_DEST="${HOME}/Applications/Locality.app"

LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

if [[ "${DEST}" == "${DEFAULT_DEST}" && -d "${LEGACY_DEST}" ]]; then
  if command -v "${LSREGISTER}" >/dev/null 2>&1; then
    "${LSREGISTER}" -u "${LEGACY_DEST}" || true
  fi
  rm -rf "${LEGACY_DEST}"
fi

mkdir -p "$(dirname "${DEST}")"
rm -rf "${DEST}"
cp -R "${APP}" "${DEST}"

if command -v "${LSREGISTER}" >/dev/null 2>&1; then
  "${LSREGISTER}" -f "${DEST}"
fi

open -gj "${DEST}" || true
echo "${DEST}"
