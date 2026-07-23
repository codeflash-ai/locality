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

export CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-luna}"
export CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
export CODEX_EXEC_TIMEOUT_SECONDS="${CODEX_EXEC_TIMEOUT_SECONDS:-900}"
export NATURAL_RUNS="${NATURAL_RUNS:-2}"
export NATURAL_OUT_ROOT="${NATURAL_OUT_ROOT:-$REPO_DIR/experiment/runs-2}"
export LOCALITY_SOURCE_ROOT="${LOCALITY_SOURCE_ROOT:-/home/amika/notion}"

node "$SCRIPT_DIR/scripts/run-natural-retrieval-batch.mjs" "$@"
