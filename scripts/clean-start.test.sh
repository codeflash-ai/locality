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

test_registered_plugin_paths_from_match_output_extracts_unique_appex_paths() {
  local actual
  actual="$(clean_start_registered_plugin_paths_from_match_output <<'EOF'
-  F ai.codeflash.locality.Locality.FileProvider(0.1)	1D45FC5C-5FDA-4C5F-9506-689AC1FA6824	2026-07-02 17:33:59 +0000	/private/var/folders/gt/6989l91x4ls_47vdhp33t7sm0000gn/T/tmp.gEoTLYqS5S/mount/Locality.app/Contents/PlugIns/LocalityFileProvider.appex
-  F ai.codeflash.locality.Locality.FileProvider(0.1)	1D45FC5C-5FDA-4C5F-9506-689AC1FA6824	2026-07-02 17:33:59 +0000	/private/var/folders/gt/6989l91x4ls_47vdhp33t7sm0000gn/T/tmp.gEoTLYqS5S/mount/Locality.app/Contents/PlugIns/LocalityFileProvider.appex
+  F ai.codeflash.locality.Locality.FileProvider(0.1)	3D9D0683-3D14-4F95-BCDC-862C6F2CE933	2026-07-04 02:40:25 +0000	/Applications/Locality.app/Contents/PlugIns/LocalityFileProvider.appex
 (2 plug-ins)
EOF
)"
  assert_lines_equal \
    "${actual}" \
    "/private/var/folders/gt/6989l91x4ls_47vdhp33t7sm0000gn/T/tmp.gEoTLYqS5S/mount/Locality.app/Contents/PlugIns/LocalityFileProvider.appex
/Applications/Locality.app/Contents/PlugIns/LocalityFileProvider.appex"
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

test_mount_root_candidates_include_broken_cloudstorage_alias_symlinks() {
  local tmp_home actual
  tmp_home="$(mktemp -d)"
  mkdir -p "${tmp_home}/Library/CloudStorage"
  ln -s "${tmp_home}/missing" "${tmp_home}/Library/CloudStorage/Locality-Locality"
  actual="$(HOME="${tmp_home}" clean_start_mount_root_candidates)"
  assert_contains_line "${actual}" "${tmp_home}/Library/CloudStorage/Locality-Locality"
}

main() {
  test_target_app_paths_include_system_and_user_bundles
  test_target_app_paths_append_extra_bundle_without_replacing_defaults
  test_target_helper_paths_follow_all_target_bundles
  test_target_helper_paths_prefer_deduplicated_targets
  test_registered_plugin_paths_from_match_output_extracts_unique_appex_paths
  test_support_paths_include_locality_file_provider_persistence
  test_mount_root_candidates_include_existing_cloudstorage_aliases
  test_mount_root_candidates_include_broken_cloudstorage_alias_symlinks
  printf 'clean-start helper tests passed\n'
}

main "$@"
