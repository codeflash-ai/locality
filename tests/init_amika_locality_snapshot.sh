#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${ROOT}/scripts/init-amika-locality-snapshot.sh"
AZURE_SETUP_SCRIPT="${ROOT}/experiment/locality-mcp-comparison/setup-codex-azure.sh"

fail() {
  printf 'init Amika Locality snapshot test: %s\n' "$*" >&2
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

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-init-amika-snapshot-test.XXXXXX")"
trap 'rm -rf "$tmp_root"' EXIT
fake_bin="${tmp_root}/bin"
fake_log="${tmp_root}/amika.log"
profile_key_input="${tmp_root}/profile-key.input"
azure_input="${tmp_root}/azure-key.input"
prompt_input="${tmp_root}/scenario-prompt.input"
fake_report="${tmp_root}/final_report.md"
mkdir -p "$fake_bin"

cat > "${fake_bin}/codex" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "${fake_bin}/codex"

setup_agent_root="${tmp_root}/setup-agent"
setup_config="${setup_agent_root}/.codex/config.toml"
mkdir -p "$(dirname "$setup_config")"
setup_config_text="$(cat <<'TOML'
# Existing Codex settings must survive Azure setup.
"approval_policy" = "never" # keep quoted root key
"model" = "old-model" # keep target comment
"literal.key" = 'preserved literal value'

["model_providers"."azure"] # keep quoted provider table
"name" = "Old Azure name" # keep provider comment
"base_url" = "https://old.invalid/openai/v1"
"env_key" = "OLD_AZURE_KEY"
"wire_api" = "chat"
custom_setting = "preserved-provider-value"

[features]
web_search_request = true
multiline_basic = """
model = "not a real setting"
[model_providers.azure]
# this comment belongs to the string
"""
multiline_literal = '''
wire_api = "also not a real setting"
'''
preserved_array = [
  "first",
  "second", # keep array comment
]

["quoted.table"]
"quoted.key" = "preserved without a trailing newline"
TOML
)"
printf '%s' "$setup_config_text" > "$setup_config"
unset setup_config_text

PATH="${fake_bin}:$PATH" \
  CODEX_HOME="${setup_agent_root}/.codex" \
  AMIKA_AGENT_CWD="$setup_agent_root" \
  CODEX_MODEL="merged-model" \
  CODEX_REASONING_EFFORT="high" \
  AZURE_OPENAI_BASE_URL="https://merged.invalid/openai/v1" \
  "$AZURE_SETUP_SCRIPT" >/dev/null

python3 - "$setup_config" <<'PY'
import os
import stat
import sys
import tomllib

path = sys.argv[1]
with open(path, "rb") as source:
    contents = source.read()
config = tomllib.loads(contents.decode("utf-8"))
assert config["model"] == "merged-model"
assert config["model_provider"] == "azure"
assert config["model_reasoning_effort"] == "high"
assert config["sandbox_mode"] == "workspace-write"
assert config["approval_policy"] == "never"
assert config["features"]["web_search_request"] is True
assert config["literal.key"] == "preserved literal value"
assert 'model = "not a real setting"' in config["features"]["multiline_basic"]
assert '[model_providers.azure]' in config["features"]["multiline_basic"]
assert 'wire_api = "also not a real setting"' in config["features"]["multiline_literal"]
assert config["features"]["preserved_array"] == ["first", "second"]
assert config["quoted.table"]["quoted.key"] == "preserved without a trailing newline"
provider = config["model_providers"]["azure"]
assert provider["name"] == "Azure OpenAI"
assert provider["base_url"] == "https://merged.invalid/openai/v1"
assert provider["env_key"] == "AZURE_OPENAI_API_KEY"
assert provider["wire_api"] == "responses"
assert provider["custom_setting"] == "preserved-provider-value"
assert stat.S_IMODE(os.stat(path).st_mode) == 0o600
assert not contents.endswith((b"\n", b"\r"))
text = contents.decode("utf-8")
assert '# Existing Codex settings must survive Azure setup.' in text
assert '"approval_policy" = "never" # keep quoted root key' in text
assert '"model" = "merged-model" # keep target comment' in text
assert '["model_providers"."azure"] # keep quoted provider table' in text
assert '"name" = "Azure OpenAI" # keep provider comment' in text
assert '''multiline_basic = """
model = "not a real setting"
[model_providers.azure]
# this comment belongs to the string
"""''' in text
assert '''multiline_literal = \'\'\'
wire_api = "also not a real setting"
\'\'\'''' in text
assert '''preserved_array = [
  "first",
  "second", # keep array comment
]''' in text
PY

inline_setup_root="${tmp_root}/inline-setup"
inline_setup_config="${inline_setup_root}/config.toml"
mkdir -p "$inline_setup_root"
printf '%s' 'model = "old-model"
model_providers = { azure = { name = "inline Azure" } }' > "$inline_setup_config"
cp "$inline_setup_config" "${inline_setup_config}.expected"
set +e
inline_setup_output="$(
  env -u AMIKA_AGENT_CWD \
    PATH="${fake_bin}:$PATH" \
    CODEX_HOME="$inline_setup_root" \
    CODEX_MODEL="merged-model" \
    CODEX_REASONING_EFFORT="high" \
    AZURE_OPENAI_BASE_URL="https://merged.invalid/openai/v1" \
    "$AZURE_SETUP_SCRIPT" 2>&1
)"
inline_setup_status=$?
set -e
[ "$inline_setup_status" -ne 0 ] || fail "inline Azure provider table should fail closed"
cmp -s "$inline_setup_config" "${inline_setup_config}.expected" || \
  fail "failed inline Azure provider merge changed the original config"
grep -F -q -- 'refusing to modify inline Azure provider setting' <<<"$inline_setup_output" || \
  fail "inline Azure provider failure did not explain why the merge was refused"

cat > "${fake_bin}/amika" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

printf 'amika' >> "${FAKE_AMIKA_LOG:?}"
for arg in "$@"; do
  printf ' %q' "$arg" >> "$FAKE_AMIKA_LOG"
done
printf '\n' >> "$FAKE_AMIKA_LOG"

if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "create" ]; then
  if [ -n "${FAKE_CREATE_STATUS:-}" ]; then
    exit "$FAKE_CREATE_STATUS"
  fi
  printf 'created\n'
  exit 0
fi

if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "ssh" ]; then
  [ -z "${AMIKA_SECRET_LINE:-}" ] || {
    printf 'secret environment leak\n' >&2
    exit 90
  }
  [ -z "${AZURE_OPENAI_API_KEY:-}" ] || {
    printf 'Azure key environment leak\n' >&2
    exit 91
  }
  last_arg=""
  for arg in "$@"; do
    last_arg="$arg"
  done
  encoded="${last_arg##* }"
  decoded="$(printf '%s' "$encoded" | base64 -d | tr '\0' '\n')"
  decoded_log="$(printf '%s' "$encoded" | base64 -d | tr '\0' ' ')"
  printf 'remote %s\n' "$decoded_log" >> "$FAKE_AMIKA_LOG"
  if [ -n "${FAKE_SIGNAL_MATCH:-}" ] && grep -F -q -- "$FAKE_SIGNAL_MATCH" <<<"$decoded_log"; then
    kill -TERM "$$"
  fi
  if [ -n "${FAKE_BLOCK_MATCH:-}" ] && grep -F -q -- "$FAKE_BLOCK_MATCH" <<<"$decoded_log"; then
    trap '' HUP INT TERM
    sleep 300 &
    blocking_child=$!
    printf '%s %s %s\n' "$PPID" "$$" "$blocking_child" > "${FAKE_BLOCK_PID_FILE:?}"
    wait "$blocking_child"
  fi
  case "$decoded_log" in
    *"--profile-key-stdin"*)
      failures="${FAKE_PRE_SENTINEL_FAILURES:-0}"
      count=0
      if [ -n "${FAKE_PRE_SENTINEL_COUNT:-}" ] && [ -f "$FAKE_PRE_SENTINEL_COUNT" ]; then
        count="$(cat "$FAKE_PRE_SENTINEL_COUNT")"
      fi
      count=$((count + 1))
      if [ -n "${FAKE_PRE_SENTINEL_COUNT:-}" ]; then
        printf '%s\n' "$count" > "$FAKE_PRE_SENTINEL_COUNT"
      fi
      if [ "$count" -le "$failures" ]; then
        exit 255
      fi
      printf '__LOCALITY_STDIN_READY__\n'
      IFS= read -r token
      printf '%s\n' "$token" > "${FAKE_PROFILE_KEY_INPUT:?}"
      if [ -n "${FAKE_POST_SENTINEL_STATUS:-}" ]; then
        exit "$FAKE_POST_SENTINEL_STATUS"
      fi
      printf '{"ok":true,"command":"sandbox_init","root":"/workspace/scoped"}\n'
      ;;
    *"base64 -d > /home/amika/scenario-prompt.md"*)
      prompt_base64="$(printf '%s\n' "$decoded" | tail -n 1)"
      printf '%s' "$prompt_base64" | base64 -d > "${FAKE_PROMPT_INPUT:?}"
      ;;
    *"codex exec"*)
      printf '__LOCALITY_STDIN_READY__\n'
      IFS= read -r azure_key
      printf '%s\n' "$azure_key" > "${FAKE_AZURE_INPUT:?}"
      printf '# Fake Launch Gate Memo\n\nVerified report body.\n' > "${FAKE_REPORT:?}"
      ;;
    *"cat /home/amika/final_report.md"*)
      cat "${FAKE_REPORT:?}"
      ;;
    *)
      printf 'installed\n'
      ;;
  esac
  exit 0
fi

printf 'unexpected fake amika command: %s\n' "$*" >&2
exit 2
SH
chmod +x "${fake_bin}/amika"

profile_key="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
azure_key="test-azure-key"
output="$(
  printf '%s\n' "$profile_key" | \
    PATH="${fake_bin}:$PATH" \
    AZURE_OPENAI_API_KEY="$azure_key" \
    FAKE_AMIKA_LOG="$fake_log" \
    FAKE_PROFILE_KEY_INPUT="$profile_key_input" \
    FAKE_AZURE_INPUT="$azure_input" \
    FAKE_PROMPT_INPUT="$prompt_input" \
    FAKE_REPORT="$fake_report" \
    "$SCRIPT" \
      --api-url https://api.dev.locality.dev \
      --name test-snapshot \
      --model test-model \
      --reasoning medium
)"

assert_contains "$fake_log" "sandbox create --remote --name test-snapshot --no-git --yes"
assert_contains "$fake_log" "sandbox ssh -t test-snapshot"
assert_contains "$fake_log" "Locality_Linux_v"
assert_contains "$fake_log" "0.3.7"
assert_contains "$fake_log" "692b05460839ba44b85cd1e6b3b6969ad4a3f62f3e81f420c4651159ad7ef195"
assert_contains "$fake_log" "sha256sum -c"
assert_contains "$fake_log" "dpkg-deb -x"
if grep -F -q -- 'cargo build' "$fake_log"; then
  fail "released CLI workflow should not build loc from source"
fi
assert_contains "$fake_log" '.local/bin/loc'
assert_contains "$fake_log" "sandbox init"
assert_contains "$fake_log" "--api-url https://api.dev.locality.dev"
assert_contains "$fake_log" "--root /home/amika/locality-snapshot"
assert_contains "$fake_log" "--profile-key-stdin"
assert_contains "$fake_log" "--profile"
assert_contains "$fake_log" "/home/amika/scenario-prompt.md"
assert_contains "$fake_log" "setup-codex-azure.sh"
assert_contains "$fake_log" 'AMIKA_AGENT_CWD="$HOME"'
assert_contains "$fake_log" "codex exec"
assert_contains "$fake_log" '< /dev/null'
assert_contains "$fake_log" "test-model medium /home/amika/locality-snapshot"
assert_contains "$fake_log" "cat /home/amika/final_report.md"
assert_contains "$fake_log" "status --porcelain --untracked-files=all"
profile_exchange_line="$(grep -n -m1 -- '--profile-key-stdin' "$fake_log" | cut -d: -f1)"
repo_prepare_line="$(grep -n -m1 -- 'git clone https://github.com/codeflash-ai/locality.git' "$fake_log" | cut -d: -f1)"
[ "$profile_exchange_line" -lt "$repo_prepare_line" ] || \
  fail "scenario repository must be prepared only after profile authorization succeeds"
if grep -F -q -- "$profile_key" "$fake_log"; then
  fail "Workspace Profile key leaked into Amika arguments"
fi
if grep -F -q -- "$azure_key" "$fake_log"; then
  fail "Azure API key leaked into Amika arguments"
fi
[ "$(cat "$profile_key_input")" = "$profile_key" ] || fail "Workspace Profile key was not streamed to loc"
[ "$(cat "$azure_input")" = "$azure_key" ] || fail "Azure API key was not streamed to the sandbox"
assert_contains "$prompt_input" '/home/amika/locality-snapshot'
assert_contains "$prompt_input" '/home/amika/workspace/locality'
assert_contains "$prompt_input" '/home/amika/final_report.md'
if grep -E -q -- '(^|[^[:alnum:]_])~[/]|/home/ubuntu/' "$prompt_input"; then
  fail "effective prompt contains a path outside /home/amika"
fi
grep -F -q -- 'Snapshot ready in Amika sandbox test-snapshot at /home/amika/locality-snapshot' <<<"$output" || \
  fail "success output did not identify the sandbox and root"
grep -F -q -- '===== Inline scenario prompt =====' <<<"$output" || \
  fail "terminal output did not include the scenario prompt heading"
grep -F -q -- '===== /home/amika/final_report.md =====' <<<"$output" || \
  fail "terminal output did not include the report heading"
grep -F -q -- 'Verified report body.' <<<"$output" || \
  fail "terminal output did not include final_report.md"

: > "$fake_log"
set +e
reuse_output="$(
  PATH="${fake_bin}:$PATH" \
    AZURE_OPENAI_API_KEY="$azure_key" \
    FAKE_AMIKA_LOG="$fake_log" \
    "$SCRIPT" \
      --api-url https://api.dev.locality.dev \
      --name existing-snapshot \
      --reuse </dev/null 2>&1
)"
reuse_status=$?
set -e
[ "$reuse_status" -eq 2 ] || fail "--reuse should be refused with status 2"
[ ! -s "$fake_log" ] || fail "refused --reuse contacted Amika"
grep -F -q -- 'existing sandbox is not a trusted credential boundary' <<<"$reuse_output" || \
  fail "refused --reuse did not explain the trust boundary"

: > "$fake_log"
set +e
create_failure_output="$(
  printf '%s\n' "$profile_key" | \
    PATH="${fake_bin}:$PATH" \
    AZURE_OPENAI_API_KEY="$azure_key" \
    FAKE_AMIKA_LOG="$fake_log" \
    FAKE_CREATE_STATUS=17 \
    "$SCRIPT" \
      --api-url https://api.dev.locality.dev \
      --name colliding-snapshot 2>&1
)"
create_failure_status=$?
set -e
[ "$create_failure_status" -eq 2 ] || fail "fresh sandbox creation failure should fail closed"
assert_contains "$fake_log" "sandbox create --remote --name colliding-snapshot --no-git --yes"
assert_not_contains "$fake_log" "sandbox ssh"
grep -F -q -- 'no credentials were transferred' <<<"$create_failure_output" || \
  fail "sandbox collision failure did not explain credential safety"

: > "$fake_log"
pre_sentinel_count="${tmp_root}/pre-sentinel.count"
retry_output="$(
  printf '%s\n' "$profile_key" | \
    PATH="${fake_bin}:$PATH" \
    AZURE_OPENAI_API_KEY="$azure_key" \
    FAKE_AMIKA_LOG="$fake_log" \
    FAKE_PROFILE_KEY_INPUT="$profile_key_input" \
    FAKE_AZURE_INPUT="$azure_input" \
    FAKE_PROMPT_INPUT="$prompt_input" \
    FAKE_REPORT="$fake_report" \
    FAKE_PRE_SENTINEL_FAILURES=1 \
    FAKE_PRE_SENTINEL_COUNT="$pre_sentinel_count" \
    "$SCRIPT" \
      --api-url https://api.dev.locality.dev \
      --name retry-snapshot \
      --model test-model \
      --reasoning medium 2>&1
)"
[ "$(cat "$pre_sentinel_count")" -eq 2 ] || fail "pre-sentinel transport failure was not retried exactly once"
grep -F -q -- 'retrying (1/3)' <<<"$retry_output" || fail "pre-sentinel retry was not explained"
[ "$(cat "$profile_key_input")" = "$profile_key" ] || fail "retry did not stream the Workspace Profile key"

: > "$fake_log"
set +e
signal_output="$(
  printf '%s\n' "$profile_key" | \
    PATH="${fake_bin}:$PATH" \
    AZURE_OPENAI_API_KEY="$azure_key" \
    FAKE_AMIKA_LOG="$fake_log" \
    FAKE_PROFILE_KEY_INPUT="$profile_key_input" \
    FAKE_AZURE_INPUT="$azure_input" \
    FAKE_PROMPT_INPUT="$prompt_input" \
    FAKE_REPORT="$fake_report" \
    FAKE_SIGNAL_MATCH='Locality_Linux_v' \
    "$SCRIPT" --api-url https://api.dev.locality.dev --name signaled 2>&1
)"
signal_status=$?
set -e
[ "$signal_status" -eq 143 ] || fail "signaled Amika child should return 143, got ${signal_status}: ${signal_output}"

: > "$fake_log"
block_pid_file="${tmp_root}/blocked-child.pids"
interrupt_output="${tmp_root}/interrupt.output"
interrupt_input="${tmp_root}/interrupt.input"
printf '%s\n' "$profile_key" > "$interrupt_input"
PATH="${fake_bin}:$PATH" \
  AZURE_OPENAI_API_KEY="$azure_key" \
  FAKE_AMIKA_LOG="$fake_log" \
  FAKE_PROFILE_KEY_INPUT="$profile_key_input" \
  FAKE_AZURE_INPUT="$azure_input" \
  FAKE_PROMPT_INPUT="$prompt_input" \
  FAKE_REPORT="$fake_report" \
  FAKE_BLOCK_MATCH='Locality_Linux_v' \
  FAKE_BLOCK_PID_FILE="$block_pid_file" \
  "$SCRIPT" --api-url https://api.dev.locality.dev --name interrupted \
    <"$interrupt_input" >"$interrupt_output" 2>&1 &
public_script_pid=$!
for _ in $(seq 1 100); do
  [ -s "$block_pid_file" ] && break
  sleep 0.05
done
[ -s "$block_pid_file" ] || fail "interruption fixture did not start the remote child"
read -r blocked_expect_pid blocked_amika_pid blocked_descendant_pid < "$block_pid_file"
kill -TERM "$public_script_pid"
set +e
wait "$public_script_pid"
interrupt_status=$?
set -e
[ "$interrupt_status" -eq 143 ] || \
  fail "TERM-interrupted public script should return 143, got ${interrupt_status}: $(cat "$interrupt_output")"
for _ in $(seq 1 100); do
  if ! kill -0 "$blocked_expect_pid" 2>/dev/null && \
     ! kill -0 "$blocked_amika_pid" 2>/dev/null && \
     ! kill -0 "$blocked_descendant_pid" 2>/dev/null; then
    break
  fi
  sleep 0.05
done
if kill -0 "$blocked_expect_pid" 2>/dev/null; then
  fail "interrupted public script left Expect wrapper ${blocked_expect_pid} running"
fi
if kill -0 "$blocked_amika_pid" 2>/dev/null; then
  fail "interrupted public script left Amika child ${blocked_amika_pid} running"
fi
if kill -0 "$blocked_descendant_pid" 2>/dev/null; then
  fail "interrupted public script left descendant ${blocked_descendant_pid} running"
fi

: > "$fake_log"
post_sentinel_count="${tmp_root}/post-sentinel.count"
set +e
post_sentinel_output="$(
  printf '%s\n' "$profile_key" | \
    PATH="${fake_bin}:$PATH" \
    AZURE_OPENAI_API_KEY="$azure_key" \
    FAKE_AMIKA_LOG="$fake_log" \
    FAKE_PROFILE_KEY_INPUT="$profile_key_input" \
    FAKE_AZURE_INPUT="$azure_input" \
    FAKE_PROMPT_INPUT="$prompt_input" \
    FAKE_REPORT="$fake_report" \
    FAKE_PRE_SENTINEL_COUNT="$post_sentinel_count" \
    FAKE_POST_SENTINEL_STATUS=23 \
    "$SCRIPT" --api-url https://api.dev.locality.dev --name post-sentinel 2>&1
)"
post_sentinel_status=$?
set -e
[ "$post_sentinel_status" -eq 23 ] || fail "post-sentinel failure should preserve status 23"
[ "$(cat "$post_sentinel_count")" -eq 1 ] || fail "post-sentinel failure must not retry"
if grep -F -q -- 'credential transport closed' <<<"$post_sentinel_output"; then
  fail "post-sentinel failure was incorrectly classified as retryable transport"
fi

: > "$fake_log"
set +e
missing_azure_output="$(
  printf '%s\n' "$profile_key" | \
    env -u AZURE_OPENAI_API_KEY \
      PATH="${fake_bin}:$PATH" \
      FAKE_AMIKA_LOG="$fake_log" \
      FAKE_PROFILE_KEY_INPUT="$profile_key_input" \
      FAKE_AZURE_INPUT="$azure_input" \
      FAKE_PROMPT_INPUT="$prompt_input" \
      FAKE_REPORT="$fake_report" \
      "$SCRIPT" --api-url https://api.dev.locality.dev --name missing-azure 2>&1
)"
missing_azure_status=$?
set -e
[ "$missing_azure_status" -eq 2 ] || fail "missing Azure key should return usage status 2"
[ ! -s "$fake_log" ] || fail "missing Azure key should fail before creating a sandbox"
grep -F -q -- 'AZURE_OPENAI_API_KEY is required' <<<"$missing_azure_output" || \
  fail "missing Azure key error was not actionable"

: > "$fake_log"
set +e
invalid_output="$(
  printf '\n' | \
    PATH="${fake_bin}:$PATH" \
    AZURE_OPENAI_API_KEY="$azure_key" \
    FAKE_AMIKA_LOG="$fake_log" \
    FAKE_PROFILE_KEY_INPUT="$profile_key_input" \
    FAKE_AZURE_INPUT="$azure_input" \
    FAKE_PROMPT_INPUT="$prompt_input" \
    FAKE_REPORT="$fake_report" \
    "$SCRIPT" --api-url https://api.dev.locality.dev --name invalid-key 2>&1
)"
invalid_status=$?
set -e
[ "$invalid_status" -eq 2 ] || fail "empty Workspace Profile key should return usage status 2"
[ ! -s "$fake_log" ] || fail "empty Workspace Profile key should fail before creating a sandbox"
grep -F -q -- 'Workspace Profile key must be 64 lowercase hexadecimal characters' <<<"$invalid_output" || \
  fail "empty Workspace Profile key error was not actionable"

printf 'init Amika Locality snapshot tests passed\n'
