#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${ROOT}/scripts/init-amika-locality-snapshot.sh"

fail() {
  printf 'init Amika Locality snapshot test: %s\n' "$*" >&2
  exit 1
}

assert_contains() {
  local path="$1"
  local needle="$2"
  grep -F -q -- "$needle" "$path" || fail "missing ${needle} in ${path}"
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

cat > "${fake_bin}/amika" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

printf 'amika' >> "${FAKE_AMIKA_LOG:?}"
for arg in "$@"; do
  printf ' %q' "$arg" >> "$FAKE_AMIKA_LOG"
done
printf '\n' >> "$FAKE_AMIKA_LOG"

if [ "${1:-}" = "sandbox" ] && [ "${2:-}" = "create" ]; then
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

assert_contains "$fake_log" "sandbox create --remote --name test-snapshot --yes"
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
assert_contains "$fake_log" "codex exec"
assert_contains "$fake_log" '< /dev/null'
assert_contains "$fake_log" "test-model medium /home/amika/locality-snapshot"
assert_contains "$fake_log" "cat /home/amika/final_report.md"
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
reuse_output="$(
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
      --name existing-snapshot \
      --reuse \
      --model test-model \
      --reasoning medium
)"
assert_contains "$fake_log" "sandbox ssh -t existing-snapshot"
if grep -F -q -- 'sandbox create' "$fake_log"; then
  fail "--reuse must not create another sandbox"
fi
grep -F -q -- 'Reusing Amika sandbox existing-snapshot' <<<"$reuse_output" || \
  fail "reuse output did not identify the existing sandbox"

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
      --reuse \
      --model test-model \
      --reasoning medium 2>&1
)"
[ "$(cat "$pre_sentinel_count")" -eq 2 ] || fail "pre-sentinel transport failure was not retried exactly once"
grep -F -q -- 'retrying (1/3)' <<<"$retry_output" || fail "pre-sentinel retry was not explained"
[ "$(cat "$profile_key_input")" = "$profile_key" ] || fail "retry did not stream the Workspace Profile key"

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

: > "$fake_log"
set +e
implicit_reuse_output="$(
  printf '%s\n' "$profile_key" | \
    PATH="${fake_bin}:$PATH" \
    AZURE_OPENAI_API_KEY="$azure_key" \
    FAKE_AMIKA_LOG="$fake_log" \
    "$SCRIPT" --api-url https://api.dev.locality.dev --reuse 2>&1
)"
implicit_reuse_status=$?
set -e
[ "$implicit_reuse_status" -eq 2 ] || fail "--reuse without --name should return usage status 2"
[ ! -s "$fake_log" ] || fail "invalid --reuse should fail before contacting Amika"
grep -F -q -- '--reuse requires an explicit --name' <<<"$implicit_reuse_output" || \
  fail "invalid --reuse error was not actionable"

printf 'init Amika Locality snapshot tests passed\n'
