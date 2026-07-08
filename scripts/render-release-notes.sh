#!/usr/bin/env bash
set -euo pipefail

ROOT="${RELEASE_NOTES_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
ROOT="$(cd "${ROOT}" && pwd)"
OUTPUT="${RELEASE_NOTES_OUTPUT:-${ROOT}/target/release/release-notes.md}"
RELEASE_TAG="${RELEASE_TAG:-${1:-}}"
PREVIOUS_TAG="${RELEASE_NOTES_PREVIOUS_TAG:-}"
CODEX_BIN="${RELEASE_NOTES_CODEX_BIN:-codex}"
MODEL="${RELEASE_NOTES_MODEL:-}"
MAX_COMMITS="${RELEASE_NOTES_MAX_COMMITS:-250}"
ALLOW_FALLBACK="${RELEASE_NOTES_ALLOW_FALLBACK:-0}"

log() {
  printf 'release-notes: %s\n' "$*"
}

fail() {
  printf 'release-notes: error: %s\n' "$*" >&2
  exit 1
}

require_git_ref() {
  local ref="$1"
  git -C "${ROOT}" rev-parse --verify "${ref}^{commit}" >/dev/null 2>&1
}

if [[ -z "${RELEASE_TAG}" ]]; then
  fail "set RELEASE_TAG or pass a release tag as the first argument"
fi

if ! require_git_ref "${RELEASE_TAG}"; then
  if git -C "${ROOT}" remote get-url origin >/dev/null 2>&1; then
    git -C "${ROOT}" fetch --force --tags origin >/dev/null 2>&1 || true
  fi
fi

require_git_ref "${RELEASE_TAG}" || fail "release tag ${RELEASE_TAG} does not exist"

RELEASE_COMMIT="$(git -C "${ROOT}" rev-parse "${RELEASE_TAG}^{commit}")"
APP_VERSION="${APP_VERSION:-${RELEASE_TAG#v}}"
REPO_SLUG="${GITHUB_REPOSITORY:-}"
if [[ -z "${REPO_SLUG}" ]]; then
  origin_url="$(git -C "${ROOT}" remote get-url origin 2>/dev/null || true)"
  case "${origin_url}" in
    git@github.com:*.git)
      REPO_SLUG="${origin_url#git@github.com:}"
      REPO_SLUG="${REPO_SLUG%.git}"
      ;;
    https://github.com/*.git)
      REPO_SLUG="${origin_url#https://github.com/}"
      REPO_SLUG="${REPO_SLUG%.git}"
      ;;
    https://github.com/*)
      REPO_SLUG="${origin_url#https://github.com/}"
      ;;
  esac
fi

if [[ -z "${PREVIOUS_TAG}" ]]; then
  if git -C "${ROOT}" rev-parse --verify "${RELEASE_COMMIT}^" >/dev/null 2>&1; then
    PREVIOUS_TAG="$(git -C "${ROOT}" describe --tags --abbrev=0 --match 'v[0-9]*' "${RELEASE_COMMIT}^" 2>/dev/null || true)"
  fi
else
  require_git_ref "${PREVIOUS_TAG}" || fail "previous tag ${PREVIOUS_TAG} does not exist"
fi

if [[ -n "${PREVIOUS_TAG}" ]]; then
  RANGE="${PREVIOUS_TAG}..${RELEASE_COMMIT}"
  RANGE_LABEL="${PREVIOUS_TAG}..${RELEASE_TAG}"
  COMPARE_URL=""
  if [[ -n "${REPO_SLUG}" ]]; then
    COMPARE_URL="https://github.com/${REPO_SLUG}/compare/${PREVIOUS_TAG}...${RELEASE_TAG}"
  fi
else
  RANGE="${RELEASE_COMMIT}"
  RANGE_LABEL="initial history through ${RELEASE_TAG}"
  COMPARE_URL=""
  if [[ -n "${REPO_SLUG}" ]]; then
    COMPARE_URL="https://github.com/${REPO_SLUG}/releases/tag/${RELEASE_TAG}"
  fi
fi

COMMIT_COUNT="$(git -C "${ROOT}" rev-list --count "${RANGE}")"
mkdir -p "$(dirname "${OUTPUT}")"

tmp_dir="$(mktemp -d "${TMPDIR:-/tmp}/loc-release-notes.XXXXXX")"
cleanup() {
  rm -rf "${tmp_dir}"
}
trap cleanup EXIT

if [[ -n "${CODEX_CONFIG_TOML:-}" ]]; then
  codex_home="${RELEASE_NOTES_CODEX_HOME:-${tmp_dir}/codex-home}"
  mkdir -p "${codex_home}"
  printf '%s\n' "${CODEX_CONFIG_TOML}" >"${codex_home}/config.toml"
  export CODEX_HOME="${codex_home}"
fi

context_file="${tmp_dir}/release-context.md"
prompt_file="${tmp_dir}/prompt.md"
codex_output="${tmp_dir}/codex-release-notes.md"

{
  printf '# Release metadata\n\n'
  printf -- '- Product: Locality\n'
  printf -- '- Version: %s\n' "${APP_VERSION}"
  printf -- '- Release tag: %s\n' "${RELEASE_TAG}"
  printf -- '- Release commit: %s\n' "${RELEASE_COMMIT}"
  if [[ -n "${PREVIOUS_TAG}" ]]; then
    printf -- '- Previous tag: %s\n' "${PREVIOUS_TAG}"
  else
    printf -- '- Previous tag: none found\n'
  fi
  printf -- '- Git range: %s\n' "${RANGE_LABEL}"
  printf -- '- Commit count: %s\n' "${COMMIT_COUNT}"
  if [[ -n "${COMPARE_URL}" ]]; then
    printf -- '- Full changelog URL: %s\n' "${COMPARE_URL}"
  fi
  printf '\n# Diff stat\n\n'
  if [[ -n "${PREVIOUS_TAG}" ]]; then
    git -C "${ROOT}" diff --stat "${PREVIOUS_TAG}" "${RELEASE_COMMIT}" || true
  else
    git -C "${ROOT}" diff-tree --stat --root -r "${RELEASE_COMMIT}" || true
  fi
  printf '\n# Changed files\n\n'
  if [[ -n "${PREVIOUS_TAG}" ]]; then
    git -C "${ROOT}" diff --name-status "${PREVIOUS_TAG}" "${RELEASE_COMMIT}" || true
  else
    git -C "${ROOT}" diff-tree --name-status --root -r "${RELEASE_COMMIT}" || true
  fi
  printf '\n# Commits\n\n'
  git -C "${ROOT}" log \
    --reverse \
    --max-count="${MAX_COMMITS}" \
    --date=short \
    --name-status \
    --pretty=format:'## %h%nFull SHA: %H%nAuthor: %an%nDate: %ad%nSubject: %s%n%nBody:%n%b%n%nFiles:' \
    "${RANGE}" || true
  if [[ "${COMMIT_COUNT}" -gt "${MAX_COMMITS}" ]]; then
    printf '\n\n# Truncation notice\n\nOnly the newest %s commits are included above out of %s total commits.\n' "${MAX_COMMITS}" "${COMMIT_COUNT}"
  fi
} >"${context_file}"

cat >"${prompt_file}" <<'PROMPT'
You are writing GitHub Release notes for Locality.

Locality turns remote systems of record, especially Notion, into local files that
agents and people can read, edit, review, and safely push back. The audience for
these notes is technical users evaluating whether to install or update Locality.

Use the release context from stdin. Treat direct commits and merged pull requests
as equally important. Do not imply that only PRs count. Do not fabricate changes.
Do not mention internal implementation details unless they materially affect users,
installers, release operators, or agent workflows.

Return only Markdown for the release body. Do not wrap it in a code fence.

Preferred structure:

## What's Changed

Group the important changes into concise, user-meaningful bullets. Merge related
commits into one bullet when that produces a clearer release note.

## Upgrade Notes

Include this section only when the commits mention compatibility, migration,
state, installer, daemon, File Provider, sync, or operator actions.

## Fixes and Polish

Include this section only when there are smaller fixes worth calling out.

## Full Changelog

Include the full changelog URL from the release metadata when one is provided.
PROMPT

render_fallback_notes() {
  {
    printf "## What's Changed\n\n"
    if [[ "${COMMIT_COUNT}" == "0" ]]; then
      printf -- "- No commits were found in %s.\n" "${RANGE_LABEL}"
    else
      git -C "${ROOT}" log --reverse --max-count=20 --pretty=format:'- %s (%h)' "${RANGE}"
      printf '\n'
      if [[ "${COMMIT_COUNT}" -gt 20 ]]; then
        printf -- "- Plus %s additional commits.\n" "$((COMMIT_COUNT - 20))"
      fi
    fi
    if [[ -n "${COMPARE_URL}" ]]; then
      printf "\n## Full Changelog\n\n%s\n" "${COMPARE_URL}"
    fi
  } >"${OUTPUT}"
}

codex_args=(--ask-for-approval never exec --ephemeral --sandbox read-only -C "${ROOT}")
if [[ -n "${MODEL}" ]]; then
  codex_args+=(-m "${MODEL}")
fi
codex_args+=("$(cat "${prompt_file}")")

log "generating notes for ${RANGE_LABEL}"
if ! "${CODEX_BIN}" "${codex_args[@]}" <"${context_file}" >"${codex_output}"; then
  if [[ "${ALLOW_FALLBACK}" == "1" ]]; then
    log "Codex failed; writing commit-subject fallback notes"
    render_fallback_notes
    exit 0
  fi
  fail "Codex failed to generate release notes"
fi

sed '/^[[:space:]]*```markdown[[:space:]]*$/d; /^[[:space:]]*```[[:space:]]*$/d' "${codex_output}" >"${OUTPUT}"

if [[ ! -s "${OUTPUT}" ]]; then
  if [[ "${ALLOW_FALLBACK}" == "1" ]]; then
    log "Codex returned empty notes; writing commit-subject fallback notes"
    render_fallback_notes
    exit 0
  fi
  fail "Codex returned empty release notes"
fi

if [[ -n "${COMPARE_URL}" ]] && ! grep -F -q "${COMPARE_URL}" "${OUTPUT}"; then
  {
    printf '\n## Full Changelog\n\n'
    printf '%s\n' "${COMPARE_URL}"
  } >>"${OUTPUT}"
fi

log "wrote ${OUTPUT}"
