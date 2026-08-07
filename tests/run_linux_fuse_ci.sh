#!/usr/bin/env bash
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: tests/run_linux_fuse_ci.sh <command> [args ...]" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
IMAGE="${LOCALITY_LINUX_FUSE_CI_IMAGE:-locality-linux-fuse-ci:rust-1.96.0}"
target_parent="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
target_root="$(mktemp -d "$target_parent/locality-fuse-target.XXXXXX")"
cleanup() {
  rm -rf "$target_root"
}
trap cleanup EXIT

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required for the privileged Linux FUSE CI environment" >&2
  exit 1
fi

docker build \
  --file "$ROOT/tests/linux-fuse-ci.Dockerfile" \
  --tag "$IMAGE" \
  "$ROOT/tests"

docker_args=(
  run
  --rm
  --privileged
  --device /dev/fuse
  --security-opt apparmor=unconfined
  --env "LOCALITY_CI_UID=$(id -u)"
  --env "LOCALITY_CI_GID=$(id -g)"
  --env "LOCALITY_BIN=/tmp/locality-target/debug/loc"
  --env "LOCALITYD_BIN=/tmp/locality-target/debug/localityd"
  --env "LOCALITY_FUSE_BIN=/tmp/locality-target/debug/locality-fuse"
  --volume "$ROOT:/workspace"
  --volume "$target_root:/tmp/locality-target"
  --workdir /workspace
)

if [[ -d "$HOME/.cargo/registry" ]]; then
  docker_args+=(--volume "$HOME/.cargo/registry:/tmp/locality-cargo/registry")
fi
if [[ -d "$HOME/.cargo/git" ]]; then
  docker_args+=(--volume "$HOME/.cargo/git:/tmp/locality-cargo/git")
fi
if [[ -d "$HOME/.loc/credentials" ]]; then
  docker_args+=(--volume "$HOME/.loc/credentials:/tmp/locality-home/.loc/credentials")
fi
if [[ -n "${RUNNER_TEMP:-}" && -d "$RUNNER_TEMP" ]]; then
  docker_args+=(--volume "$RUNNER_TEMP:$RUNNER_TEMP")
fi

forwarded_env=(
  GRANOLA_API_KEY
  LINEAR_API_KEY
  LOCALITY_FUSE_SMOKE
  LOCALITY_FUSE_SMOKE_REQUIRED
  LOCALITY_GMAIL_LIVE_CREDENTIAL_JSON
  LOCALITY_GMAIL_LIVE_TO_EMAIL
  LOCALITY_GOOGLE_CALENDAR_LIVE_CREDENTIAL_JSON
  LOCALITY_GOOGLE_DOCS_LIVE_CREDENTIAL_JSON
  LOCALITY_GOOGLE_DOCS_LIVE_WORKSPACE_FOLDER
  LOCALITY_GRANOLA_LIVE_NOTE_ID
  LOCALITY_LINEAR_LIVE_ISSUE_ID
  LOCALITY_LIVE_FORCE_OAUTH_REFRESH
  LOCALITY_LIVE_GMAIL_SEND
  LOCALITY_LIVE_GMAIL_VFS
  LOCALITY_LIVE_GOOGLE_CALENDAR_VFS
  LOCALITY_LIVE_GOOGLE_DOCS_VFS
  LOCALITY_LIVE_GRANOLA_VFS
  LOCALITY_LIVE_LINEAR_VFS
  LOCALITY_LIVE_NOTION_VFS_PUSH_PULL
  LOCALITY_LIVE_ROTATED_CREDENTIAL_OUTPUT
  LOCALITY_LIVE_SLACK_VFS
  LOCALITY_NOTION_LIVE_PARENT_PAGE
  LOCALITY_SLACK_LIVE_CONVERSATION_ID
  LOCALITY_SLACK_LIVE_CREDENTIAL_JSON
  LOCALITY_SLACK_LIVE_TYPES
  NOTION_AT
  NOTION_TOKEN
)

for name in "${forwarded_env[@]}"; do
  if [[ -n "${!name+x}" ]]; then
    docker_args+=(--env "$name")
  fi
done

docker "${docker_args[@]}" "$IMAGE" "$@"
