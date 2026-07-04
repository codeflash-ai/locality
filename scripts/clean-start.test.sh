#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./clean-start-lib.sh
source "${ROOT}/scripts/clean-start-lib.sh"

fail() {
  printf 'clean-start test failed: %s\n' "$*" >&2
  exit 1
}

assert_lines_equal() {
  local actual="$1"
  local expected="$2"
  [[ "${actual}" == "${expected}" ]] || fail "expected lines:\n${expected}\nactual lines:\n${actual}"
}

assert_contains_line() {
  local haystack="$1"
  local needle="$2"
  grep -Fqx "${needle}" <<<"${haystack}" || fail "missing line: ${needle}"
}

test_target_app_paths_include_system_and_user_bundles() {
  local tmp_home
  tmp_home="$(mktemp -d)"
  HOME="${tmp_home}" assert_lines_equal \
    "$(HOME="${tmp_home}" clean_start_target_app_paths)" \
    "/Applications/Locality.app
${tmp_home}/Applications/Locality.app"
}

test_target_app_paths_append_extra_bundle_without_replacing_defaults() {
  local tmp_home extra_app
  tmp_home="$(mktemp -d)"
  extra_app="${tmp_home}/Downloads/Locality.app"
  HOME="${tmp_home}" assert_lines_equal \
    "$(HOME="${tmp_home}" clean_start_target_app_paths "${extra_app}")" \
    "/Applications/Locality.app
${tmp_home}/Applications/Locality.app
${extra_app}"
}

test_target_helper_paths_follow_all_target_bundles() {
  local tmp_home extra_app actual
  tmp_home="$(mktemp -d)"
  extra_app="${tmp_home}/Custom/Locality.app"
  actual="$(HOME="${tmp_home}" clean_start_target_helper_paths "${extra_app}")"
  assert_contains_line "${actual}" "/Applications/Locality.app/Contents/MacOS/locality-file-providerctl"
  assert_contains_line "${actual}" "${tmp_home}/Applications/Locality.app/Contents/MacOS/locality-file-providerctl"
  assert_contains_line "${actual}" "${extra_app}/Contents/MacOS/locality-file-providerctl"
}

test_target_helper_paths_prefer_deduplicated_targets() {
  local tmp_home actual
  tmp_home="$(mktemp -d)"
  actual="$(HOME="${tmp_home}" clean_start_target_helper_paths "${tmp_home}/Applications/Locality.app")"
  assert_lines_equal \
    "${actual}" \
    "/Applications/Locality.app/Contents/MacOS/locality-file-providerctl
${tmp_home}/Applications/Locality.app/Contents/MacOS/locality-file-providerctl"
}

test_support_paths_include_locality_file_provider_persistence() {
  local tmp_home actual
  tmp_home="$(mktemp -d)"
  actual="$(HOME="${tmp_home}" clean_start_support_paths)"
  assert_contains_line \
    "${actual}" \
    "${tmp_home}/Library/Application Support/FileProvider/ai.codeflash.locality.Locality.FileProvider"
}

test_mount_root_candidates_include_existing_cloudstorage_aliases() {
  local tmp_home actual
  tmp_home="$(mktemp -d)"
  mkdir -p "${tmp_home}/Library/CloudStorage/Locality-Locality"
  mkdir -p "${tmp_home}/Library/CloudStorage/Locality-notion-main"
  actual="$(HOME="${tmp_home}" clean_start_mount_root_candidates)"
  assert_contains_line "${actual}" "${tmp_home}/Library/CloudStorage/Locality"
  assert_contains_line "${actual}" "${tmp_home}/Library/CloudStorage/Locality-Locality"
  assert_contains_line "${actual}" "${tmp_home}/Library/CloudStorage/Locality-notion-main"
}

main() {
  test_target_app_paths_include_system_and_user_bundles
  test_target_app_paths_append_extra_bundle_without_replacing_defaults
  test_target_helper_paths_follow_all_target_bundles
  test_target_helper_paths_prefer_deduplicated_targets
  test_support_paths_include_locality_file_provider_persistence
  test_mount_root_candidates_include_existing_cloudstorage_aliases
  printf 'clean-start helper tests passed\n'
}

main "$@"
