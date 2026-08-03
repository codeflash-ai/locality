#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: init-amika-locality-snapshot.sh --api-url <origin> [options]

Creates a fresh remote Amika sandbox, builds the Locality CLI from this checkout, and
materializes a scoped workspace snapshot. It then runs one inline Notion-only
scenario and prints both the prompt and generated report. The one-time bootstrap
token is read from standard input and is never passed in a command-line argument.

Options:
  --api-url <origin>       Locality backend API origin (required).
  --name <name>            Amika sandbox name. Default: locality-snapshot-<UTC>.
  --model <model>          Model passed to codex exec.
                           Default: CODEX_MODEL or gpt-5.6-sol.
  --reasoning <effort>     Reasoning effort passed to codex exec.
                           Default: CODEX_REASONING_EFFORT or low.
  --help, -h               Show this help.

Example:
  printf '%s\n' "$LOCALITY_BOOTSTRAP_TOKEN" | \
    scripts/init-amika-locality-snapshot.sh \
      --api-url https://api.dev.locality.dev
EOF
}

fail() {
  printf 'init Amika Locality snapshot: %s\n' "$*" >&2
  exit 2
}

API_URL=""
SANDBOX_NAME="locality-snapshot-$(date -u +%Y%m%d-%H%M%S)"
REMOTE_ROOT="/home/amika/locality-snapshot"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
AZURE_OPENAI_BASE_URL="${AZURE_OPENAI_BASE_URL:-https://aseem-mp32maxp-eastus2.openai.azure.com/openai/v1}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --api-url)
      [ "$#" -ge 2 ] || fail "--api-url requires a value"
      API_URL="$2"
      shift 2
      ;;
    --name)
      [ "$#" -ge 2 ] || fail "--name requires a value"
      SANDBOX_NAME="$2"
      shift 2
      ;;
    --model)
      [ "$#" -ge 2 ] || fail "--model requires a value"
      CODEX_MODEL="$2"
      shift 2
      ;;
    --reasoning)
      [ "$#" -ge 2 ] || fail "--reasoning requires a value"
      CODEX_REASONING_EFFORT="$2"
      shift 2
      ;;
    --help|-h)
      usage
      exit 0
      ;;
    *)
      fail "unknown argument: $1"
      ;;
  esac
done

[ -n "$API_URL" ] || fail "--api-url is required"
case "$API_URL" in
  https://*) ;;
  *) fail "--api-url must use https" ;;
esac
case "$SANDBOX_NAME" in
  *[!a-zA-Z0-9._-]*|'') fail "--name contains unsupported characters" ;;
esac
[ -n "$CODEX_MODEL" ] || fail "--model must not be empty"
[ -n "${AZURE_OPENAI_API_KEY:-}" ] || fail "AZURE_OPENAI_API_KEY is required"
case "$AZURE_OPENAI_BASE_URL" in
  https://*) ;;
  *) fail "AZURE_OPENAI_BASE_URL must use https" ;;
esac
case "$CODEX_REASONING_EFFORT" in
  low|medium|high|xhigh|max|ultra) ;;
  *) fail "--reasoning must be low, medium, high, xhigh, max, or ultra" ;;
esac

scenario_prompt() {
  cat <<'EOF'
Use the scoped Notion workspace snapshot at `/home/amika/locality-snapshot` for all Notion context.
Use the repository checkout at `/home/amika/workspace/locality`.
Write the final report to `/home/amika/final_report.md`.

You are preparing a launch gate memo for Locality. Find the relevant Notion
context under `/home/amika/locality-snapshot` and recent code changes from
`/home/amika/workspace/locality`, decide what is actually proven, what is still
unverified, and what should block launch. Produce a concise Markdown memo.

Do not use direct Notion API tools in this run. Do not create a new Notion page
or modify existing Notion pages.

Write the final Markdown report to `/home/amika/final_report.md`.

Report format:

# Locality Launch Gate Memo

## Recommendation

## Evidence Reviewed

## Proven

## Unverified

## Launch Blockers

## Required Validation

The memo should be concise, specific, and grounded in evidence. If a claim
cannot be verified from git, gh, or Locality context, say so.
EOF
}

EFFECTIVE_PROMPT="$(scenario_prompt)"

command -v amika >/dev/null 2>&1 || fail "amika is not available on PATH"
command -v git >/dev/null 2>&1 || fail "git is not available on PATH"
SOURCE_REVISION="$(git -C "$REPO_ROOT" rev-parse HEAD)" || fail "could not resolve source revision"

IFS= read -r BOOTSTRAP_TOKEN || fail "read the bootstrap token from standard input"
[ -n "$BOOTSTRAP_TOKEN" ] || fail "bootstrap token must not be empty"

printf 'Creating Amika sandbox %s...\n' "$SANDBOX_NAME"
(cd "$REPO_ROOT" && amika sandbox create \
  --remote \
  --name "$SANDBOX_NAME" \
  --yes >/dev/null)

printf 'Building loc CLI at revision %s in %s...\n' "$SOURCE_REVISION" "$SANDBOX_NAME"
amika sandbox ssh "$SANDBOX_NAME" -- sh -c '
  set -eu
  revision=$1
  manifest=$(find "$HOME/workspace" -mindepth 2 -maxdepth 2 -type f -name Cargo.toml -print -quit)
  test -n "$manifest"
  repo_dir=${manifest%/Cargo.toml}
  git -C "$repo_dir" cat-file -e "$revision^{commit}"
  git -C "$repo_dir" checkout --detach "$revision"
  if ! command -v cargo >/dev/null 2>&1; then
    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs |
      sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"
  fi
  cargo build --manifest-path "$manifest" --release -p loc-cli
  if [ "$repo_dir" != /home/amika/workspace/locality ]; then
    test ! -e /home/amika/workspace/locality
    ln -s "$repo_dir" /home/amika/workspace/locality
  fi
  mkdir -p "$HOME/.local/bin"
  install -m 0755 "$repo_dir/target/release/loc" "$HOME/.local/bin/loc"
  "$HOME/.local/bin/loc" sandbox init --help >/dev/null
' sh "$SOURCE_REVISION"

printf 'Materializing scoped workspace at %s:%s...\n' "$SANDBOX_NAME" "$REMOTE_ROOT"
printf '%s\n' "$BOOTSTRAP_TOKEN" | amika sandbox ssh "$SANDBOX_NAME" -- \
  sh -c 'exec "$HOME/.local/bin/loc" sandbox init "$@"' sh \
    --api-url "$API_URL" \
    --root "$REMOTE_ROOT" \
    --bootstrap-token-stdin \
    --json
unset BOOTSTRAP_TOKEN

printf 'Snapshot ready in Amika sandbox %s at %s\n' "$SANDBOX_NAME" "$REMOTE_ROOT"

amika sandbox ssh "$SANDBOX_NAME" -- sh -c '
  set -eu
  azure_base_url=$1
  model=$2
  reasoning=$3
  AZURE_OPENAI_BASE_URL="$azure_base_url" \
    CODEX_MODEL="$model" \
    CODEX_REASONING_EFFORT="$reasoning" \
    bash /home/amika/workspace/locality/experiment/locality-mcp-comparison/setup-codex-azure.sh
' sh "$AZURE_OPENAI_BASE_URL" "$CODEX_MODEL" "$CODEX_REASONING_EFFORT"

printf '%s\n' "$EFFECTIVE_PROMPT" | amika sandbox ssh "$SANDBOX_NAME" -- \
  sh -c 'cat > /home/amika/scenario-prompt.md'

printf '\n===== Inline scenario prompt =====\n%s\n' "$EFFECTIVE_PROMPT"
printf '\n===== Running scenario in %s =====\n' "$SANDBOX_NAME"

printf '%s\n' "$AZURE_OPENAI_API_KEY" | amika sandbox ssh "$SANDBOX_NAME" -- sh -c '
  set -eu
  model=$1
  reasoning=$2
  snapshot_root=$3
  IFS= read -r azure_api_key
  test -n "$azure_api_key"
  command -v codex >/dev/null 2>&1 || {
    printf "codex is not installed in the Amika sandbox\n" >&2
    exit 127
  }
  rm -f /home/amika/final_report.md /home/amika/scenario-codex.jsonl
  prompt=$(cat /home/amika/scenario-prompt.md)
  AZURE_OPENAI_API_KEY="$azure_api_key" codex exec \
    --json \
    --model "$model" \
    -c "model_reasoning_effort=\"$reasoning\"" \
    --dangerously-bypass-approvals-and-sandbox \
    --disable hooks \
    -C /home/amika/workspace/locality \
    --add-dir "$snapshot_root" \
    "$prompt" > /home/amika/scenario-codex.jsonl
  unset azure_api_key
  test -s /home/amika/final_report.md || {
    printf "scenario did not create /home/amika/final_report.md\n" >&2
    exit 1
  }
' sh "$CODEX_MODEL" "$CODEX_REASONING_EFFORT" "$REMOTE_ROOT"

printf '\n===== /home/amika/final_report.md =====\n'
amika sandbox ssh "$SANDBOX_NAME" -- cat /home/amika/final_report.md
