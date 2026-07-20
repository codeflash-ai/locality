#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="${REPO_DIR:-/home/amika/workspace/locality}"
cd "$REPO_DIR"

export PATH="$HOME/.cargo/bin:$PATH"

ENV_FILE="${LOCALITY_EXPERIMENT_ENV:-$HOME/.config/locality-experiment/env}"
if [ -f "$ENV_FILE" ]; then
  set -a
  # shellcheck disable=SC1090
  source "$ENV_FILE"
  set +a
fi

if [ -z "${AZURE_OPENAI_API_KEY:-}" ]; then
  cat >&2 <<'EOF'
AZURE_OPENAI_API_KEY is missing in the sandbox.
Run the key setup command from your local machine before starting the experiment.
EOF
  exit 2
fi

export TARGET_URL="${TARGET_URL:-https://app.notion.com/p/codeflash/Amika-Test-Update-45a3ac0ebb888265b97301c156aeb9ef}"
export CONTEXT_URLS="${CONTEXT_URLS:-https://app.notion.com/p/codeflash/Locality-Launch-Amika-Environment-3a33ac0ebb888001ac26d52f57f1deba}"
export CONTEXT_SEARCH_QUERY="${CONTEXT_SEARCH_QUERY:-benchmark|launch readiness|safe diff|push|review|Live Mode|File Provider|Windows Cloud Files|distribution|Homebrew|install|connector|standup|blocker|risk}"
export CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-sol}"
export CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"

export EXPERIMENT_DIR="$SCRIPT_DIR"

"$SCRIPT_DIR/run-launch-readiness-benchmark.sh" --compare-mcp "$@"
