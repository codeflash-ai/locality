#!/usr/bin/env bash
set -euo pipefail

RUNS="${RUNS:-5}"
CODEX_MODEL="${CODEX_MODEL:-gpt-5.6-luna}"
CODEX_REASONING_EFFORT="${CODEX_REASONING_EFFORT:-low}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BASE_OUT_DIR="${BASE_OUT_DIR:-$REPO_ROOT/target/launch-readiness-amika-repeated}"

cd "$REPO_ROOT"

for i in $(seq 1 "$RUNS"); do
  run_id="$(date -u +%Y%m%dT%H%M%SZ)-$i"
  echo "== run $i/$RUNS: $run_id model=$CODEX_MODEL effort=$CODEX_REASONING_EFFORT =="
  RUN_ID="$run_id" \
  LOCAL_OUT_DIR="$BASE_OUT_DIR/$run_id" \
  REPORT_TITLE="Launch Readiness Benchmark $run_id" \
  CODEX_MODEL="$CODEX_MODEL" \
  CODEX_REASONING_EFFORT="$CODEX_REASONING_EFFORT" \
    "$SCRIPT_DIR/run-agent-comparison.sh"
done

python3 "$SCRIPT_DIR/scripts/summarize-runs.py" "$BASE_OUT_DIR"
