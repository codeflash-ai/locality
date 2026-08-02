#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUNNER="${ROOT}/experiment/locality-mcp-comparison/run-launch-readiness-benchmark.sh"

fail() {
  printf 'launch readiness prompt path test: %s\n' "$*" >&2
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

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-launch-readiness-prompt-paths.XXXXXX")"
cleanup() {
  rm -rf "$tmp_root"
}
trap cleanup EXIT

fake_bin="${tmp_root}/bin"
mkdir -p "$fake_bin"

cat > "${fake_bin}/codex" <<'SH'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  exec)
    output_last_message=""
    previous=""
    for arg in "$@"; do
      if [ "$previous" = "--output-last-message" ]; then
        output_last_message="$arg"
      fi
      previous="$arg"
    done
    mkdir -p "$(dirname "${REPORT_FILE:?}")"
    printf 'locality report\n' > "$REPORT_FILE"
    if [ -n "$output_last_message" ]; then
      printf 'final\n' > "$output_last_message"
    fi
    printf '{"type":"turn.started"}\n'
    printf '{"type":"turn.completed"}\n'
    ;;
  *)
    printf 'unexpected fake codex command: %s\n' "$*" >&2
    exit 2
    ;;
esac
SH
chmod +x "${fake_bin}/codex"

fake_loc="${tmp_root}/loc"
cat > "$fake_loc" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf 'fake loc\n'
SH
chmod +x "$fake_loc"

repo="${tmp_root}/repo"
mkdir -p "$repo"
git -C "$repo" init -q
git -C "$repo" config user.email test@example.com
git -C "$repo" config user.name "Test User"
printf 'initial\n' > "$repo/README.md"
printf 'test agent instructions\n' > "$repo/AGENTS.md"
git -C "$repo" add README.md AGENTS.md
git -C "$repo" commit -q -m "Initial commit"

prompt_root="${tmp_root}/prompts"
mkdir -p "$prompt_root/Locality"
cat > "$prompt_root/Locality/scenario1.md" <<'PROMPT'
Use `~/workspace/locality`, `~/Locality/notion`, and `/home/ubuntu/notion`.
Do not write under `/home/amika/Locality`.
Write the final Markdown report to `/home/ubuntu/final_report.md`.
PROMPT

home_dir="${tmp_root}/home"
sandbox_home="${tmp_root}/amika-home"
out_dir="${tmp_root}/out"
mkdir -p "$home_dir" "$sandbox_home"

PATH="${fake_bin}:$PATH" \
  HOME="$home_dir" \
  REPO_DIR="$repo" \
  LOC_BIN="$fake_loc" \
  PROMPT_ROOT="$prompt_root" \
  OUT_DIR="$out_dir" \
  SANDBOX_HOME="$sandbox_home" \
  AGENT_REPORT_PATH="$sandbox_home/final_report.md" \
  CODEX_EXEC_TIMEOUT_SECONDS=0 \
  CODEX_MODEL="fake-model" \
  CODEX_REASONING_EFFORT="low" \
  "$RUNNER" --strategy locality --scenario scenario1 >/dev/null

prompt_snapshot="${out_dir}/scenarios/scenario1/locality-prompt.md"
assert_contains "$prompt_snapshot" "${sandbox_home}/workspace/locality"
assert_contains "$prompt_snapshot" "${sandbox_home}/Locality/notion"
assert_contains "$prompt_snapshot" "${sandbox_home}/notion"
assert_contains "$prompt_snapshot" "${sandbox_home}/final_report.md"
assert_not_contains "$prompt_snapshot" "/home/ubuntu"
assert_not_contains "$prompt_snapshot" "/home/amika"
assert_not_contains "$prompt_snapshot" "~/"
assert_contains "${out_dir}/scenarios/scenario1/locality-codex-command.txt" "report_source=${sandbox_home}/final_report.md"

printf 'launch readiness prompt path tests passed\n'
