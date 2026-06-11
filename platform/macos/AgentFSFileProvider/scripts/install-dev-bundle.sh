#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="$("${ROOT}/scripts/build-dev-bundle.sh")"
DEST="${AGENTFS_APP_DEST:-${HOME}/Applications/AgentFS.app}"

mkdir -p "$(dirname "${DEST}")"
rm -rf "${DEST}"
cp -R "${APP}" "${DEST}"

if command -v /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister >/dev/null 2>&1; then
  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f "${DEST}"
fi

open -gj "${DEST}" || true
echo "${DEST}"
