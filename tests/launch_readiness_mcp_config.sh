#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SETUP_SCRIPT="${ROOT}/experiment/locality-mcp-comparison/setup-codex-azure.sh"
RUNNER="${ROOT}/experiment/locality-mcp-comparison/run-launch-readiness-benchmark.sh"

fail() {
  printf 'launch readiness MCP config test: %s\n' "$*" >&2
  exit 1
}

assert_contains() {
  local path="$1"
  local needle="$2"
  grep -F -q -- "$needle" "$path" || fail "missing ${needle} in ${path}"
}

assert_not_contains() {
  local path="$1"
  local needle="$2"
  if grep -F -q -- "$needle" "$path"; then
    fail "unexpected ${needle} in ${path}"
  fi
}

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-launch-readiness-mcp-test.XXXXXX")"
cleanup() {
  rm -rf "$tmp_root"
}
trap cleanup EXIT

fake_bin="${tmp_root}/bin"
mkdir -p "$fake_bin"
fake_log="${tmp_root}/codex.log"

cat >"${fake_bin}/codex" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

log="${FAKE_CODEX_LOG:?}"
codex_home="${CODEX_HOME:-$HOME/.codex}"

case "${1:-}" in
  --version)
    printf 'fake-codex 0.0.0\n'
    ;;
  mcp)
    action="${2:-}"
    name="${3:-}"
    printf 'mcp %s %s home=%s\n' "$action" "$name" "$codex_home" >> "$log"
    mkdir -p "$codex_home"
    touch "$codex_home/config.toml"
    if [ "$action" = "add" ]; then
      {
        printf '\n'
        printf '[mcp_servers.%s]\n' "$name"
        printf 'fake = true\n'
      } >> "$codex_home/config.toml"
    fi
    ;;
  exec)
    output_last_message=""
    previous=""
    for arg in "$@"; do
      if [ "$previous" = "--output-last-message" ]; then
        output_last_message="$arg"
      fi
      previous="$arg"
    done

    has_mcp=0
    if grep -F -q '[mcp_servers' "$codex_home/config.toml"; then
      has_mcp=1
    fi

    if [[ "$output_last_message" == *locality-agent-final.md ]]; then
      printf 'exec locality home=%s has_mcp=%s out_dir=%s report_file=%s trace_file=%s git_data_file=%s artifact_out_dir=%s hardcoded_out_dir=%s\n' \
        "$codex_home" "$has_mcp" "$OUT_DIR" "${REPORT_FILE:-}" "${TRACE_FILE:-}" "${GIT_DATA_FILE:-}" "${ARTIFACT_OUT_DIR:-}" "${CODEX_SANDBOX_HARDCODED_OUT_DIR:-}" >> "$log"
      if [ "$has_mcp" -ne 0 ]; then
        printf 'Locality Codex home contains MCP config\n' >&2
        exit 31
      fi
      if [ "${FAKE_CODEX_WRITE_HARDCODED:-0}" = "1" ]; then
        report_path="${CODEX_SANDBOX_HARDCODED_OUT_DIR:?}/report-body.md"
        trace_path="${CODEX_SANDBOX_HARDCODED_OUT_DIR:?}/locality-agent-trace.md"
      else
        report_path="${REPORT_FILE:-$OUT_DIR/report-body.md}"
        trace_path="${TRACE_FILE:-$OUT_DIR/locality-agent-trace.md}"
      fi
      mkdir -p "$(dirname "$report_path")" "$(dirname "$trace_path")"
      printf 'locality report\n' > "$report_path"
      printf 'locality trace\n' > "$trace_path"
    else
      printf 'exec notion-mcp home=%s has_mcp=%s out_dir=%s report_file=%s trace_file=%s git_data_file=%s artifact_out_dir=%s hardcoded_out_dir=%s\n' \
        "$codex_home" "$has_mcp" "$OUT_DIR" "${REPORT_FILE:-}" "${TRACE_FILE:-}" "${GIT_DATA_FILE:-}" "${ARTIFACT_OUT_DIR:-}" "${CODEX_SANDBOX_HARDCODED_OUT_DIR:-}" >> "$log"
      if [ "$has_mcp" -ne 1 ]; then
        printf 'MCP Codex home is missing MCP config\n' >&2
        exit 32
      fi
      if [ "${FAKE_CODEX_WRITE_HARDCODED:-0}" = "1" ]; then
        report_path="${CODEX_SANDBOX_HARDCODED_OUT_DIR:?}/notion-mcp-report-body.md"
        trace_path="${CODEX_SANDBOX_HARDCODED_OUT_DIR:?}/notion-mcp-agent-trace.md"
      else
        report_path="${REPORT_FILE:-$OUT_DIR/notion-mcp-report-body.md}"
        trace_path="${TRACE_FILE:-$OUT_DIR/notion-mcp-agent-trace.md}"
      fi
      mkdir -p "$(dirname "$report_path")" "$(dirname "$trace_path")"
      printf 'mcp report\n' > "$report_path"
      printf 'mcp trace\n' > "$trace_path"
    fi

    if [ -n "$output_last_message" ]; then
      printf 'final\n' > "$output_last_message"
    fi
    printf '{"type":"turn.started"}\n'
    printf '{"type":"item.started","item":{"id":"tool-1","type":"command_execution","command":"git status"}}\n'
    printf '{"type":"item.completed","item":{"id":"tool-1","type":"command_execution","command":"git status"}}\n'
    printf '{"type":"turn.completed"}\n'
    ;;
  *)
    printf 'unexpected fake codex command: %s\n' "$*" >&2
    exit 2
    ;;
esac
SH
chmod +x "${fake_bin}/codex"

setup_home="${tmp_root}/setup-codex"
PATH="${fake_bin}:$PATH" \
  FAKE_CODEX_LOG="$fake_log" \
  CODEX_HOME="$setup_home" \
  CODEX_MODEL="fake-model" \
  CODEX_REASONING_EFFORT="low" \
  AZURE_OPENAI_BASE_URL="https://example.invalid/openai/v1" \
  "$SETUP_SCRIPT" >/dev/null

assert_contains "$setup_home/config.toml" 'model_provider = "azure"'
assert_not_contains "$setup_home/config.toml" '[mcp_servers'

repo="${tmp_root}/repo"
mkdir -p "$repo"
git -C "$repo" init -q
git -C "$repo" config user.email test@example.com
git -C "$repo" config user.name "Test User"
printf 'initial\n' > "$repo/README.md"
git -C "$repo" add README.md
git -C "$repo" commit -q -m "Initial commit"
git -C "$repo" remote add origin "$repo"

mount="${tmp_root}/mount"
mkdir -p "$mount/Target" "$mount/Context"
printf 'target page\n' > "$mount/Target/page.md"
printf 'launch readiness context\n' > "$mount/Context/page.md"

fake_loc="${tmp_root}/loc"
cat >"$fake_loc" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  locate)
    case "${2:-}" in
      *target*) printf '%s\n' "$FAKE_LOC_MOUNT/Target/page.md" ;;
      *) printf '%s\n' "$FAKE_LOC_MOUNT/Context/page.md" ;;
    esac
    ;;
  pull)
    printf 'pulled\n'
    ;;
  *)
    printf 'unexpected fake loc command: %s\n' "$*" >&2
    exit 2
    ;;
esac
SH
chmod +x "$fake_loc"

prompt_root="${tmp_root}/prompts"
mkdir -p "$prompt_root/Locality" "$prompt_root/MCP"
printf 'Read GIT_DATA_FILE. Write the Locality report to REPORT_FILE and trace to TRACE_FILE.\n' > "$prompt_root/Locality/scenario1.md"
printf 'Write the Locality report to OUT_DIR/report-body.md.\n' > "$prompt_root/Locality/scenario2.md"
printf 'Read GIT_DATA_FILE. Write the MCP report to REPORT_FILE and trace to TRACE_FILE.\n' > "$prompt_root/MCP/scenario1.md"
printf 'Write the MCP report to OUT_DIR/notion-mcp-report-body.md.\n' > "$prompt_root/MCP/scenario2.md"

base_codex_home="${tmp_root}/base-codex"
mkdir -p "$base_codex_home"
cat >"$base_codex_home/config.toml" <<'TOML'
model = "fake-model"
model_provider = "fake-provider"

[model_providers.fake-provider]
name = "Fake"
base_url = "https://example.invalid/openai/v1"
env_key = "FAKE_API_KEY"
wire_api = "responses"

[mcp_servers.stale]
url = "https://example.invalid/mcp"
TOML

out_dir="${tmp_root}/out"
sandbox_out_dir="${tmp_root}/sandbox-home"
hardcoded_out_dir="${tmp_root}/hardcoded-home"
PATH="${fake_bin}:$PATH" \
  FAKE_CODEX_LOG="$fake_log" \
  FAKE_CODEX_WRITE_HARDCODED=1 \
  FAKE_LOC_MOUNT="$mount" \
  CODEX_HOME="$base_codex_home" \
  REPO_DIR="$repo" \
  LOC_BIN="$fake_loc" \
  PROMPT_ROOT="$prompt_root" \
  OUT_DIR="$out_dir" \
  CODEX_SANDBOX_OUT_DIR="$sandbox_out_dir" \
  CODEX_SANDBOX_HARDCODED_OUT_DIR="$hardcoded_out_dir" \
  TARGET_URL="notion://target" \
  CONTEXT_URLS="notion://context" \
  CONTEXT_SEARCH_QUERY="launch readiness" \
  CODEX_EXEC_TIMEOUT_SECONDS=0 \
  CODEX_MODEL="fake-model" \
  CODEX_REASONING_EFFORT="low" \
  LINEAR_API_KEY="linear-test-token" \
  NOTION_API_TOKEN="notion-test-token" \
  "$RUNNER" --compare-mcp --scenario scenario2 >/dev/null

assert_contains "$fake_log" "exec locality home=${out_dir}/codex/locality has_mcp=0"
assert_contains "$fake_log" "mcp add linear-server home=${out_dir}/codex/notion-mcp"
assert_contains "$fake_log" "mcp add notion home=${out_dir}/codex/notion-mcp"
assert_contains "$fake_log" "exec notion-mcp home=${out_dir}/codex/notion-mcp has_mcp=1"
assert_contains "$fake_log" "exec locality home=${out_dir}/codex/locality has_mcp=0 out_dir=${sandbox_out_dir}"
assert_contains "$fake_log" "exec notion-mcp home=${out_dir}/codex/notion-mcp has_mcp=1 out_dir=${sandbox_out_dir}"
assert_not_contains "$fake_log" "mcp add notion home=${base_codex_home}"
assert_not_contains "$fake_log" "mcp add notion home=${out_dir}/codex/locality"
assert_not_contains "${out_dir}/codex/locality/config.toml" "[mcp_servers"
assert_contains "${out_dir}/codex/notion-mcp/config.toml" "[mcp_servers.notion]"
assert_contains "${out_dir}/codex/locality/hooks.json" '"PreToolUse"'
assert_contains "${out_dir}/codex/locality/hooks.json" 'codex-live-hook.py'
assert_contains "${out_dir}/scenarios.tsv" "scenario2"$'\t'"all"$'\t'"default"$'\t'"hooks"$'\t'"${prompt_root}/Locality/scenario2.md"$'\t'"${prompt_root}/MCP/scenario2.md"$'\t'"${out_dir}/scenarios/scenario2"$'\t'"${sandbox_out_dir}"
assert_contains "${out_dir}/scenarios/scenario2/report-body.md" "locality report"
assert_contains "${out_dir}/scenarios/scenario2/notion-mcp-report-body.md" "mcp report"
assert_contains "${out_dir}/scenarios/scenario2/locality-agent-artifacts.tsv" "report"$'\t'"copied"$'\t'"${hardcoded_out_dir}/report-body.md"
assert_contains "${out_dir}/scenarios/scenario2/notion-mcp-agent-artifacts.tsv" "report"$'\t'"copied"$'\t'"${hardcoded_out_dir}/notion-mcp-report-body.md"
assert_contains "${out_dir}/scenarios/scenario2/locality-codex-command.txt" "--enable hooks"
assert_contains "${out_dir}/scenarios/scenario2/locality-codex-command.txt" "--dangerously-bypass-hook-trust"
assert_contains "${out_dir}/scenarios/scenario2/locality-codex-command.txt" "agent_out_dir=${sandbox_out_dir}"
test -s "${out_dir}/scenarios/scenario2/locality.folded" || fail "missing Locality folded profile"
test -s "${out_dir}/scenarios/scenario2/locality.snakeviz.prof" || fail "missing Locality SnakeViz profile"
test -s "${out_dir}/scenarios/scenario2/locality-speedscope.json" || fail "missing Locality Speedscope profile"
test -s "${out_dir}/scenarios/scenario2/locality.perfetto.json" || fail "missing Locality Perfetto profile"
test -s "${out_dir}/scenarios/scenario2/notion-mcp.folded" || fail "missing MCP folded profile"
test -s "${out_dir}/scenarios/scenario2/notion-mcp.snakeviz.prof" || fail "missing MCP SnakeViz profile"
test -s "${out_dir}/scenarios/scenario2/notion-mcp-speedscope.json" || fail "missing MCP Speedscope profile"
test -s "${out_dir}/scenarios/scenario2/notion-mcp.perfetto.json" || fail "missing MCP Perfetto profile"
assert_contains "${out_dir}/summary.json" "profile_artifacts"

scenario1_out_dir="${tmp_root}/out-scenario1"
PATH="${fake_bin}:$PATH" \
  FAKE_CODEX_LOG="$fake_log" \
  FAKE_LOC_MOUNT="$mount" \
  CODEX_HOME="$base_codex_home" \
  REPO_DIR="$repo" \
  LOC_BIN="$fake_loc" \
  PROMPT_ROOT="$prompt_root" \
  OUT_DIR="$scenario1_out_dir" \
  TARGET_URL="notion://target" \
  CONTEXT_URLS="notion://context" \
  CONTEXT_SEARCH_QUERY="launch readiness" \
  CODEX_EXEC_TIMEOUT_SECONDS=0 \
  CODEX_MODEL="fake-model" \
  CODEX_REASONING_EFFORT="low" \
  BASE_REF="HEAD" \
  SINCE="100 years ago" \
  "$RUNNER" --scenario scenario1 >/dev/null

assert_contains "$fake_log" "exec locality home=${scenario1_out_dir}/codex/locality has_mcp=0 out_dir=${scenario1_out_dir}/scenarios/scenario1"
assert_contains "$fake_log" "report_file=${scenario1_out_dir}/scenarios/scenario1/report-body.md"
assert_contains "$fake_log" "trace_file=${scenario1_out_dir}/scenarios/scenario1/locality-agent-trace.md"
assert_contains "$fake_log" "git_data_file=${scenario1_out_dir}/scenarios/scenario1/git-data.json"
assert_contains "${scenario1_out_dir}/scenarios/scenario1/locality-codex-command.txt" "context_paths_file=${scenario1_out_dir}/scenarios/scenario1/locality-context-paths.txt"
assert_contains "${scenario1_out_dir}/scenarios/scenario1/locality-codex-command.txt" "report_file=${scenario1_out_dir}/scenarios/scenario1/report-body.md"
assert_contains "${scenario1_out_dir}/scenarios/scenario1/report-body.md" "locality report"
assert_contains "${scenario1_out_dir}/scenarios/scenario1/git-data.json" '"commit_count"'

study_out_dir="${tmp_root}/out-study"
study_sandbox_out_dir="${tmp_root}/sandbox-home-study"
PATH="${fake_bin}:$PATH" \
  FAKE_CODEX_LOG="$fake_log" \
  FAKE_LOC_MOUNT="$mount" \
  CODEX_HOME="$base_codex_home" \
  REPO_DIR="$repo" \
  LOC_BIN="$fake_loc" \
  PROMPT_ROOT="$prompt_root" \
  OUT_DIR="$study_out_dir" \
  CODEX_SANDBOX_OUT_DIR="$study_sandbox_out_dir" \
  TARGET_URL="notion://target" \
  CONTEXT_URLS="notion://context" \
  CONTEXT_SEARCH_QUERY="launch readiness" \
  CODEX_EXEC_TIMEOUT_SECONDS=0 \
  CODEX_MODEL="fake-model" \
  CODEX_REASONING_EFFORT="low" \
  LINEAR_API_KEY="linear-test-token" \
  NOTION_API_TOKEN="notion-test-token" \
  "$RUNNER" --compare-hooks --scenario scenario2 >/dev/null

assert_contains "${study_out_dir}/scenarios/scenario2/variants/locality-no-hooks/locality-codex-command.txt" "hooks_mode=no-hooks"
assert_contains "${study_out_dir}/scenarios/scenario2/variants/locality-no-hooks/locality-codex-command.txt" "agent_out_dir=${study_sandbox_out_dir}"
assert_contains "${study_out_dir}/scenarios/scenario2/variants/locality-no-hooks/locality-codex-command.txt" "--disable hooks"
assert_not_contains "${study_out_dir}/scenarios/scenario2/variants/locality-no-hooks/locality-codex-command.txt" "--dangerously-bypass-hook-trust"
assert_contains "${study_out_dir}/scenarios/scenario2/variants/locality-hooks/locality-codex-command.txt" "hooks_mode=hooks"
assert_contains "${study_out_dir}/scenarios/scenario2/variants/locality-hooks/locality-codex-command.txt" "--enable hooks"
assert_contains "${study_out_dir}/scenarios/scenario2/variants/notion-mcp-no-hooks/notion-mcp-codex-command.txt" "--disable hooks"
assert_contains "${study_out_dir}/scenarios/scenario2/variants/notion-mcp-hooks/notion-mcp-codex-command.txt" "--enable hooks"
assert_contains "${study_out_dir}/scenarios/scenario2/hooks-comparison.md" "Hook Comparison"
test -s "${study_out_dir}/scenarios/scenario2/variants/locality-hooks/locality.folded" || fail "missing hook-study folded profile"
test -s "${study_out_dir}/scenarios/scenario2/variants/locality-hooks/locality.perfetto.json" || fail "missing hook-study Perfetto profile"
test -s "${study_out_dir}/scenarios/scenario2/variants/notion-mcp-hooks/notion-mcp.snakeviz.prof" || fail "missing hook-study SnakeViz profile"
assert_contains "${study_out_dir}/summary.json" "variant_agent_event_summaries"
assert_contains "${study_out_dir}/scenarios.tsv" "hook-study"

printf 'launch readiness MCP config tests passed\n'
