#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
workdir="$(mktemp -d "${TMPDIR:-/tmp}/locality-mintlify-docs.XXXXXX")"
command="${1:-dev}"
if [[ $# -gt 0 ]]; then
  shift
fi

cleanup() {
  rm -rf "$workdir"
}
trap cleanup EXIT

mkdir -p "$workdir/docs/connectors"

cp "$repo_root/docs.json" "$workdir/docs.json"
cp "$repo_root/llms.txt" "$workdir/llms.txt"
cp "$repo_root"/docs/*.mdx "$workdir/docs/"
cp "$repo_root"/docs/connectors/*.mdx "$workdir/docs/connectors/"

echo "Prepared Mintlify docs preview at $workdir"

cd "$workdir"

case "$command" in
  dev)
    if [[ $# -eq 0 ]]; then
      set -- --no-open
    fi
    exec npx mintlify@latest dev "$@"
    ;;
  validate)
    exec npx mintlify@latest validate "$@"
    ;;
  broken-links)
    exec npx mintlify@latest broken-links "$@"
    ;;
  *)
    exec npx mintlify@latest "$command" "$@"
    ;;
esac
