#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${ROOT}/scripts/render-release-notes.sh"

fail() {
  printf 'release notes test: %s\n' "$*" >&2
  exit 1
}

[[ -x "${SCRIPT}" ]] || fail "render-release-notes.sh must be executable"

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-release-notes-test.XXXXXX")"
cleanup() {
  rm -rf "${tmp_root}"
}
trap cleanup EXIT

repo="${tmp_root}/repo"
mkdir -p "${repo}"
git -C "${repo}" init -q
git -C "${repo}" config user.name "Locality Test"
git -C "${repo}" config user.email "test@example.invalid"

printf '# Locality\n' >"${repo}/README.md"
git -C "${repo}" add README.md
git -C "${repo}" commit -q -m "Initial release"
git -C "${repo}" tag v0.1.0

mkdir -p "${repo}/docs"
printf 'Live Mode docs\n' >"${repo}/docs/live-mode.md"
git -C "${repo}" add docs/live-mode.md
git -C "${repo}" commit -q -m "Document Live Mode agent semantics"

mkdir -p "${repo}/scripts"
printf 'installer polish\n' >"${repo}/scripts/windows-installer.txt"
git -C "${repo}" add scripts/windows-installer.txt
git -C "${repo}" commit -q -m "Improve Windows installer release flow"
git -C "${repo}" tag v0.2.0

fake_codex="${tmp_root}/codex"
fake_args="${tmp_root}/codex-args.txt"
fake_stdin="${tmp_root}/codex-stdin.md"
cat >"${fake_codex}" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ -n "${CODEX_HOME:-}" ]] || { echo "CODEX_HOME was not set" >&2; exit 12; }
[[ -f "${CODEX_HOME}/config.toml" ]] || { echo "Codex config was not written" >&2; exit 13; }
grep -F -q 'model_provider = "fake-azure"' "${CODEX_HOME}/config.toml" || { echo "Codex config content was not preserved" >&2; exit 14; }
printf '%s\n' "$@" >"${FAKE_CODEX_ARGS}"
cat >"${FAKE_CODEX_STDIN}"
cat <<'NOTES'
## What's Changed

- Improved the release body generated for users and operators.
NOTES
EOF
chmod +x "${fake_codex}"

output="${tmp_root}/release-notes.md"
FAKE_CODEX_ARGS="${fake_args}" \
  FAKE_CODEX_STDIN="${fake_stdin}" \
  CODEX_CONFIG_TOML=$'model = "fake-release-model"\nmodel_provider = "fake-azure"\n\n[model_providers.fake-azure]\nname = "Fake Azure"\nbase_url = "https://example.invalid/openai/v1"\nenv_key = "AZURE_OPENAI_API_KEY"\nwire_api = "responses"' \
  GITHUB_REPOSITORY="codeflash-ai/locality" \
  RELEASE_NOTES_REPO_ROOT="${repo}" \
  RELEASE_NOTES_OUTPUT="${output}" \
  RELEASE_NOTES_CODEX_BIN="${fake_codex}" \
  RELEASE_TAG="v0.2.0" \
  "${SCRIPT}" >/dev/null

grep -F -q -- "Previous tag: v0.1.0" "${fake_stdin}" \
  || fail "script must detect the previous version tag"
grep -F -q -- "Document Live Mode agent semantics" "${fake_stdin}" \
  || fail "script must include direct commits after the previous tag"
grep -F -q -- "Improve Windows installer release flow" "${fake_stdin}" \
  || fail "script must include all commits in the tag range"
grep -F -q -- "--sandbox" "${fake_args}" \
  || fail "script must run Codex with an explicit sandbox"
grep -F -q -- "https://github.com/codeflash-ai/locality/compare/v0.1.0...v0.2.0" "${output}" \
  || fail "release notes must include the full changelog URL"
if grep -F -q '```' "${output}"; then
  fail "release notes output must not be wrapped in a code fence"
fi
