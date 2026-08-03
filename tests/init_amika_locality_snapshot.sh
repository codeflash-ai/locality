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
bootstrap_input="${tmp_root}/bootstrap-token.input"
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
  case " $* " in
    *" --bootstrap-token-stdin "*)
      IFS= read -r token
      printf '%s\n' "$token" > "${FAKE_BOOTSTRAP_INPUT:?}"
      printf '{"ok":true,"command":"sandbox_init","root":"/workspace/scoped"}\n'
      ;;
    *"base64 -d > /home/amika/scenario-prompt.md"*)
      last_arg=""
      for arg in "$@"; do
        last_arg="$arg"
      done
      printf '%s' "$last_arg" | base64 -d > "${FAKE_PROMPT_INPUT:?}"
      ;;
    *"codex exec"*)
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

bootstrap_token="test-bootstrap-token"
azure_key="test-azure-key"
output="$(
  printf '%s\n' "$bootstrap_token" | \
    PATH="${fake_bin}:$PATH" \
    AZURE_OPENAI_API_KEY="$azure_key" \
    FAKE_AMIKA_LOG="$fake_log" \
    FAKE_BOOTSTRAP_INPUT="$bootstrap_input" \
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
assert_contains "$fake_log" "sandbox ssh test-snapshot"
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
assert_contains "$fake_log" "--bootstrap-token-stdin"
assert_contains "$fake_log" "/home/amika/scenario-prompt.md"
assert_contains "$fake_log" "setup-codex-azure.sh"
assert_contains "$fake_log" "codex exec"
assert_contains "$fake_log" '< /dev/null'
assert_contains "$fake_log" "test-model medium /home/amika/locality-snapshot"
assert_contains "$fake_log" "cat /home/amika/final_report.md"
if grep -F -q -- "$bootstrap_token" "$fake_log"; then
  fail "bootstrap token leaked into Amika arguments"
fi
if grep -F -q -- "$azure_key" "$fake_log"; then
  fail "Azure API key leaked into Amika arguments"
fi
[ "$(cat "$bootstrap_input")" = "$bootstrap_token" ] || fail "bootstrap token was not streamed to loc"
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
missing_azure_output="$(
  printf '%s\n' "$bootstrap_token" | \
    env -u AZURE_OPENAI_API_KEY \
      PATH="${fake_bin}:$PATH" \
      FAKE_AMIKA_LOG="$fake_log" \
      FAKE_BOOTSTRAP_INPUT="$bootstrap_input" \
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
    FAKE_BOOTSTRAP_INPUT="$bootstrap_input" \
    FAKE_AZURE_INPUT="$azure_input" \
    FAKE_PROMPT_INPUT="$prompt_input" \
    FAKE_REPORT="$fake_report" \
    "$SCRIPT" --api-url https://api.dev.locality.dev --name invalid-key 2>&1
)"
invalid_status=$?
set -e
[ "$invalid_status" -eq 2 ] || fail "empty bootstrap token should return usage status 2"
[ ! -s "$fake_log" ] || fail "empty bootstrap token should fail before creating a sandbox"
grep -F -q -- 'bootstrap token must not be empty' <<<"$invalid_output" || \
  fail "empty bootstrap token error was not actionable"

printf 'init Amika Locality snapshot tests passed\n'
