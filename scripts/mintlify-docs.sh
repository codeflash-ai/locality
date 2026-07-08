#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
site_root="$repo_root/docs-site"
mintlify_package="${MINTLIFY_PACKAGE:-mintlify@4.2.671}"
command="${1:-dev}"
if [[ $# -gt 0 ]]; then
  shift
fi

echo "Using Mintlify docs site at $site_root"

cd "$site_root"

case "$command" in
  dev)
    if [[ $# -eq 0 ]]; then
      set -- --no-open
    fi
    exec npx "$mintlify_package" dev "$@"
    ;;
  validate)
    exec npx "$mintlify_package" validate "$@"
    ;;
  broken-links)
    exec npx "$mintlify_package" broken-links "$@"
    ;;
  *)
    exec npx "$mintlify_package" "$command" "$@"
    ;;
esac
