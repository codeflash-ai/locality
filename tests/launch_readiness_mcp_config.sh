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
      printf 'exec locality home=%s has_mcp=%s\n' "$codex_home" "$has_mcp" >> "$log"
      if [ "$has_mcp" -ne 0 ]; then
        printf 'Locality Codex home contains MCP config\n' >&2
        exit 31
      fi
      printf 'locality report\n' > "$OUT_DIR/report-body.md"
    else
      printf 'exec notion-mcp home=%s has_mcp=%s\n' "$codex_home" "$has_mcp" >> "$log"
      if [ "$has_mcp" -ne 1 ]; then
        printf 'MCP Codex home is missing MCP config\n' >&2
        exit 32
      fi
      printf 'mcp report\n' > "$OUT_DIR/notion-mcp-report-body.md"
    fi

    if [ -n "$output_last_message" ]; then
      printf 'final\n' > "$output_last_message"
    fi
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
printf 'Write the Locality report to OUT_DIR/report-body.md.\n' > "$prompt_root/Locality/scenario2.md"
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
PATH="${fake_bin}:$PATH" \
  FAKE_CODEX_LOG="$fake_log" \
  FAKE_LOC_MOUNT="$mount" \
  CODEX_HOME="$base_codex_home" \
  REPO_DIR="$repo" \
  LOC_BIN="$fake_loc" \
  PROMPT_ROOT="$prompt_root" \
  OUT_DIR="$out_dir" \
  TARGET_URL="notion://target" \
  CONTEXT_URLS="notion://context" \
  CONTEXT_SEARCH_QUERY="launch readiness" \
  CODEX_EXEC_TIMEOUT_SECONDS=0 \
  CODEX_MODEL="fake-model" \
  CODEX_REASONING_EFFORT="low" \
  LINEAR_API_KEY="linear-test-token" \
  NOTION_API_TOKEN="notion-test-token" \
  "$RUNNER" --compare-mcp >/dev/null

assert_contains "$fake_log" "exec locality home=${out_dir}/codex/locality has_mcp=0"
assert_contains "$fake_log" "mcp add linear-server home=${out_dir}/codex/notion-mcp"
assert_contains "$fake_log" "mcp add notion home=${out_dir}/codex/notion-mcp"
assert_contains "$fake_log" "exec notion-mcp home=${out_dir}/codex/notion-mcp has_mcp=1"
assert_not_contains "$fake_log" "mcp add notion home=${base_codex_home}"
assert_not_contains "$fake_log" "mcp add notion home=${out_dir}/codex/locality"
assert_not_contains "${out_dir}/codex/locality/config.toml" "[mcp_servers"
assert_contains "${out_dir}/codex/notion-mcp/config.toml" "[mcp_servers.notion]"

printf 'launch readiness MCP config tests passed\n'
