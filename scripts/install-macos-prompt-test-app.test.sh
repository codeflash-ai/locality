#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRIPT="${ROOT}/scripts/install-macos-prompt-test-app.sh"

fail() {
  printf 'install-macos-prompt-test-app test failed: %s\n' "$*" >&2
  exit 1
}

assert_contains() {
  local haystack="$1"
  local needle="$2"
  grep -Fq -- "${needle}" <<<"${haystack}" || fail "missing: ${needle}"
}

assert_not_contains() {
  local haystack="$1"
  local needle="$2"
  ! grep -Fq -- "${needle}" <<<"${haystack}" || fail "unexpected: ${needle}"
}

make_source_app() {
  local app="$1"
  mkdir -p \
    "${app}/Contents/MacOS" \
    "${app}/Contents/PlugIns/LocalityFileProvider.appex/Contents/MacOS"
  touch \
    "${app}/Contents/MacOS/locality-desktop" \
    "${app}/Contents/MacOS/loc" \
    "${app}/Contents/MacOS/localityd" \
    "${app}/Contents/MacOS/locality-file-providerctl" \
    "${app}/Contents/PlugIns/LocalityFileProvider.appex/Contents/MacOS/LocalityFileProvider"
  chmod +x \
    "${app}/Contents/MacOS/locality-desktop" \
    "${app}/Contents/MacOS/loc" \
    "${app}/Contents/MacOS/localityd" \
    "${app}/Contents/MacOS/locality-file-providerctl" \
    "${app}/Contents/PlugIns/LocalityFileProvider.appex/Contents/MacOS/LocalityFileProvider"
  cat >"${app}/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleExecutable</key><string>locality-desktop</string></dict></plist>
PLIST
  cat >"${app}/Contents/PlugIns/LocalityFileProvider.appex/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>CFBundleExecutable</key><string>LocalityFileProvider</string></dict></plist>
PLIST
}

test_dry_run_plans_fresh_prompt_test_app_installation() {
  local tmp source app output
  tmp="$(mktemp -d)"
  trap '[[ -z "${tmp:-}" ]] || rm -rf "${tmp}"' RETURN
  source="${tmp}/Source Locality.app"
  app="${tmp}/Applications/Locality Prompt Test.app"
  make_source_app "${source}"

  output="$(
    LOCALITY_PROMPT_TEST_TIMESTAMP=20260714130000 \
      "${SCRIPT}" \
        --dry-run \
        --source-app "${source}" \
        --app-path "${app}" \
        --signing-identity "Developer ID Application: Test (TEAMID)" \
        --no-launch
  )"

  assert_contains "${output}" "bundle id: ai.codeflash.locality.promptfresh20260714130000"
  assert_contains "${output}" "extension bundle id: ai.codeflash.locality.promptfresh20260714130000.FileProvider"
  assert_contains "${output}" "state isolation: prompt-fresh only"
  assert_contains "${output}" "ditto"
  assert_contains "${output}" "${source}"
  assert_contains "${output}" "${app}"
  assert_contains "${output}" "CFBundleIdentifier\\ ai.codeflash.locality.promptfresh20260714130000"
  assert_contains "${output}" "CFBundleIdentifier\\ ai.codeflash.locality.promptfresh20260714130000.FileProvider"
  assert_contains "${output}" "LocalityDeveloperId.entitlements"
  assert_contains "${output}" "locality-file-providerctl"
  assert_contains "${output}" "locality-desktop"
  assert_contains "${output}" "LocalityFileProvider.entitlements"
  assert_contains "${output}" "LocalityFileProvider.appex"
  assert_contains "${output}" "pluginkit -a"
  assert_contains "${output}" "locality-file-providerctl register --mount-id loc --display-name Locality\\ Prompt\\ Test --json"
  assert_not_contains "${output}" "open -a"
}

test_dry_run_resets_existing_test_app_domain_by_default() {
  local tmp source app output
  tmp="$(mktemp -d)"
  trap '[[ -z "${tmp:-}" ]] || rm -rf "${tmp}"' RETURN
  source="${tmp}/Source Locality.app"
  app="${tmp}/Applications/Locality Prompt Test.app"
  make_source_app "${source}"
  make_source_app "${app}"

  output="$(
    LOCALITY_PROMPT_TEST_TIMESTAMP=20260714130001 \
      "${SCRIPT}" \
        --dry-run \
        --source-app "${source}" \
        --app-path "${app}" \
        --signing-identity "Developer ID Application: Test (TEAMID)" \
        --no-launch
  )"

  assert_contains "${output}" "locality-file-providerctl reset --json"
  assert_contains "${output}" "pluginkit -r"
}

test_dry_run_can_skip_existing_test_app_domain_reset() {
  local tmp source app output
  tmp="$(mktemp -d)"
  trap '[[ -z "${tmp:-}" ]] || rm -rf "${tmp}"' RETURN
  source="${tmp}/Source Locality.app"
  app="${tmp}/Applications/Locality Prompt Test.app"
  make_source_app "${source}"
  make_source_app "${app}"

  output="$(
    LOCALITY_PROMPT_TEST_TIMESTAMP=20260714130002 \
      "${SCRIPT}" \
        --dry-run \
        --source-app "${source}" \
        --app-path "${app}" \
        --signing-identity "Developer ID Application: Test (TEAMID)" \
        --no-reset-domain \
        --no-launch
  )"

  assert_not_contains "${output}" "locality-file-providerctl reset --json"
  assert_contains "${output}" "pluginkit -r"
}

test_dry_run_rejects_non_app_target_path() {
  local tmp source app output status
  tmp="$(mktemp -d)"
  trap '[[ -z "${tmp:-}" ]] || rm -rf "${tmp}"' RETURN
  source="${tmp}/Source Locality.app"
  app="${tmp}/Applications/Locality Prompt Test"
  make_source_app "${source}"

  set +e
  output="$(
    LOCALITY_PROMPT_TEST_TIMESTAMP=20260714130003 \
      "${SCRIPT}" \
        --dry-run \
        --source-app "${source}" \
        --app-path "${app}" \
        --signing-identity "Developer ID Application: Test (TEAMID)" \
        --no-launch 2>&1
  )"
  status=$?
  set -e

  [[ "${status}" -ne 0 ]] || fail "invalid app path unexpectedly succeeded"
  assert_contains "${output}" "--app-path must point to a .app bundle"
}

test_dry_run_rejects_production_target_path_by_default() {
  local tmp source app output status
  tmp="$(mktemp -d)"
  trap '[[ -z "${tmp:-}" ]] || rm -rf "${tmp}"' RETURN
  source="${tmp}/Source Locality.app"
  app="${tmp}/Applications/Locality.app"
  make_source_app "${source}"

  set +e
  output="$(
    LOCALITY_PROMPT_TEST_TIMESTAMP=20260714130004 \
      "${SCRIPT}" \
        --dry-run \
        --source-app "${source}" \
        --app-path "${app}" \
        --signing-identity "Developer ID Application: Test (TEAMID)" \
        --no-launch 2>&1
  )"
  status=$?
  set -e

  [[ "${status}" -ne 0 ]] || fail "production app path unexpectedly succeeded"
  assert_contains "${output}" "--app-path points at the production app"
  assert_not_contains "${output}" "rm -rf"
}

test_dry_run_force_allows_non_test_target_path() {
  local tmp source app output
  tmp="$(mktemp -d)"
  trap '[[ -z "${tmp:-}" ]] || rm -rf "${tmp}"' RETURN
  source="${tmp}/Source Locality.app"
  app="${tmp}/Applications/Locality.app"
  make_source_app "${source}"

  output="$(
    LOCALITY_PROMPT_TEST_TIMESTAMP=20260714130005 \
      "${SCRIPT}" \
        --dry-run \
        --source-app "${source}" \
        --app-path "${app}" \
        --force-non-test-app-path \
        --signing-identity "Developer ID Application: Test (TEAMID)" \
        --no-launch
  )"

  assert_contains "${output}" "target app: ${app}"
  assert_contains "${output}" "rm -rf"
}

test_dry_run_rejects_source_app_as_target() {
  local tmp source output status
  tmp="$(mktemp -d)"
  trap '[[ -z "${tmp:-}" ]] || rm -rf "${tmp}"' RETURN
  source="${tmp}/Source Locality.app"
  make_source_app "${source}"

  set +e
  output="$(
    LOCALITY_PROMPT_TEST_TIMESTAMP=20260714130006 \
      "${SCRIPT}" \
        --dry-run \
        --source-app "${source}" \
        --app-path "${source}" \
        --force-non-test-app-path \
        --signing-identity "Developer ID Application: Test (TEAMID)" \
        --no-launch 2>&1
  )"
  status=$?
  set -e

  [[ "${status}" -ne 0 ]] || fail "source app as target unexpectedly succeeded"
  assert_contains "${output}" "--source-app and --app-path must be different paths"
  assert_not_contains "${output}" "rm -rf"
}

test_dry_run_prefers_explicit_dmg_over_built_app() {
  local tmp isolated_root isolated_script built_app dmg app output
  tmp="$(mktemp -d)"
  trap '[[ -z "${tmp:-}" ]] || rm -rf "${tmp}"' RETURN
  isolated_root="${tmp}/repo"
  isolated_script="${isolated_root}/scripts/install-macos-prompt-test-app.sh"
  built_app="${isolated_root}/target/release/bundle/macos/Locality.app"
  dmg="${tmp}/explicit.dmg"
  app="${tmp}/Applications/Locality Prompt Test.app"
  mkdir -p "$(dirname "${isolated_script}")" "${built_app}"
  cp "${SCRIPT}" "${isolated_script}"
  touch "${dmg}"

  output="$(
    LOCALITY_PROMPT_TEST_TIMESTAMP=20260714130007 \
      "${isolated_script}" \
        --dry-run \
        --dmg "${dmg}" \
        --app-path "${app}" \
        --signing-identity "Developer ID Application: Test (TEAMID)" \
        --no-launch
  )"

  assert_contains "${output}" "+ hdiutil attach ${dmg}"
  assert_not_contains "${output}" "source app: ${built_app}"
}

main() {
  test_dry_run_plans_fresh_prompt_test_app_installation
  test_dry_run_resets_existing_test_app_domain_by_default
  test_dry_run_can_skip_existing_test_app_domain_reset
  test_dry_run_rejects_non_app_target_path
  test_dry_run_rejects_production_target_path_by_default
  test_dry_run_force_allows_non_test_target_path
  test_dry_run_rejects_source_app_as_target
  test_dry_run_prefers_explicit_dmg_over_built_app
  printf 'install-macos-prompt-test-app helper tests passed\n'
}

main "$@"
