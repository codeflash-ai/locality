#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MACOS_WORKFLOW="${ROOT}/.github/workflows/release-macos.yml"
WINDOWS_WORKFLOW="${ROOT}/.github/workflows/release-windows.yml"
LINUX_WORKFLOW="${ROOT}/.github/workflows/release-linux.yml"
RELEASE_NOTES_WORKFLOW="${ROOT}/.github/workflows/release-notes.yml"
UPDATER_SCRIPT="${ROOT}/scripts/render-tauri-updater-manifest.sh"
RELEASE_NOTES_SCRIPT="${ROOT}/scripts/render-release-notes.sh"

fail() {
  printf 'release asset names test: %s\n' "$*" >&2
  exit 1
}

grep -F -q 'Locality_Mac_v${APP_VERSION}.dmg' "${MACOS_WORKFLOW}" \
  || fail "macOS release workflow must publish Locality_Mac_v<version>.dmg"
grep -F -q 'Locality_Mac.dmg' "${MACOS_WORKFLOW}" \
  || fail "macOS release workflow must publish a stable Locality_Mac.dmg alias"
grep -F -q 'Locality_Mac_Updater_v${APP_VERSION}.app.tar.gz' "${MACOS_WORKFLOW}" \
  || fail "macOS release workflow must publish a standard updater archive name"
grep -F -q 'HOMEBREW_DMG: target/release/github-assets/Locality_Mac_v${{ env.APP_VERSION }}.dmg' "${MACOS_WORKFLOW}" \
  || fail "macOS Homebrew cask must point at the standard DMG asset"
grep -F -q 'UPDATER_MACOS_AARCH64_ARTIFACT: target/release/github-assets/Locality_Mac_Updater_v${{ env.APP_VERSION }}.app.tar.gz' "${MACOS_WORKFLOW}" \
  || fail "macOS updater manifest must point at the standard updater asset"
grep -F -q -- '--notes "Release assets are still being published."' "${MACOS_WORKFLOW}" \
  || fail "macOS release workflow must create only placeholder release notes"

grep -F -q 'Locality_Windows_v$env:APP_VERSION.exe' "${WINDOWS_WORKFLOW}" \
  || fail "Windows release workflow must publish Locality_Windows_v<version>.exe"
grep -F -q 'Locality_Windows.exe' "${WINDOWS_WORKFLOW}" \
  || fail "Windows release workflow must publish a stable Locality_Windows.exe alias"
grep -F -q 'UPDATER_WINDOWS_X86_64_ARTIFACT: target/release/bundle/windows/Locality_Windows_v${{ env.APP_VERSION }}.exe' "${WINDOWS_WORKFLOW}" \
  || fail "Windows updater manifest must point at the standard installer asset"
grep -F -q -- '"--notes", "Release assets are still being published."' "${WINDOWS_WORKFLOW}" \
  || fail "Windows release workflow must create only placeholder release notes"

grep -F -q 'Locality_Linux_v${APP_VERSION}.deb' "${LINUX_WORKFLOW}" \
  || fail "Linux release workflow must publish Locality_Linux_v<version>.deb"
grep -F -q 'Locality_Linux_v${APP_VERSION}.rpm' "${LINUX_WORKFLOW}" \
  || fail "Linux release workflow must publish Locality_Linux_v<version>.rpm"
grep -F -q 'Locality_Linux_v${APP_VERSION}.AppImage' "${LINUX_WORKFLOW}" \
  || fail "Linux release workflow must publish Locality_Linux_v<version>.AppImage"
grep -F -q 'Locality_Linux.deb' "${LINUX_WORKFLOW}" \
  || fail "Linux release workflow must publish a stable Locality_Linux.deb alias"
grep -F -q 'Locality_Linux.rpm' "${LINUX_WORKFLOW}" \
  || fail "Linux release workflow must publish a stable Locality_Linux.rpm alias"
grep -F -q 'Locality_Linux.AppImage' "${LINUX_WORKFLOW}" \
  || fail "Linux release workflow must publish a stable Locality_Linux.AppImage alias"
grep -F -q 'UPDATER_LINUX_X86_64_ARTIFACT: target/release/github-assets/Locality_Linux_v${{ env.APP_VERSION }}.AppImage' "${LINUX_WORKFLOW}" \
  || fail "Linux updater manifest must point at the standard AppImage asset"
grep -F -q -- '--notes "Release assets are still being published."' "${LINUX_WORKFLOW}" \
  || fail "Linux release workflow must create only placeholder release notes"

grep -F -q 'scripts/render-release-notes.sh' "${RELEASE_NOTES_WORKFLOW}" \
  || fail "release notes workflow must generate LLM release notes"
grep -F -q -- '--notes-file "${release_notes_file}"' "${RELEASE_NOTES_WORKFLOW}" \
  || fail "release notes workflow must publish generated release notes"
grep -F -q 'CODEX_CONFIG_TOML: ${{ secrets.CODEX_CONFIG_TOML }}' "${RELEASE_NOTES_WORKFLOW}" \
  || fail "release notes workflow must expose Codex config to release notes"
grep -F -q 'AZURE_OPENAI_API_KEY: ${{ secrets.AZURE_OPENAI_API_KEY }}' "${RELEASE_NOTES_WORKFLOW}" \
  || fail "release notes workflow must expose Azure OpenAI key to release notes"
if grep -F -q 'scripts/render-release-notes.sh' "${MACOS_WORKFLOW}" "${WINDOWS_WORKFLOW}" "${LINUX_WORKFLOW}"; then
  fail "platform release workflows must not run Codex release-note generation"
fi
if grep -F -q -- '--generate-notes' "${MACOS_WORKFLOW}" "${WINDOWS_WORKFLOW}" "${LINUX_WORKFLOW}" "${RELEASE_NOTES_WORKFLOW}"; then
  fail "release workflows must not use GitHub-generated release notes"
fi

[[ -x "${RELEASE_NOTES_SCRIPT}" ]] || fail "release notes renderer must be executable"

tmp_root="$(mktemp -d "${TMPDIR:-/tmp}/loc-release-asset-names.XXXXXX")"
cleanup() {
  rm -rf "${tmp_root}"
}
trap cleanup EXIT

touch "${tmp_root}/Locality_Mac_Updater_v0.1.5.app.tar.gz"
printf 'mac-signature\n' >"${tmp_root}/Locality_Mac_Updater_v0.1.5.app.tar.gz.sig"
touch "${tmp_root}/Locality_Windows_v0.1.5.exe"
printf 'windows-signature\n' >"${tmp_root}/Locality_Windows_v0.1.5.exe.sig"
touch "${tmp_root}/Locality_Linux_v0.1.5.AppImage"
printf 'linux-signature\n' >"${tmp_root}/Locality_Linux_v0.1.5.AppImage.sig"

UPDATER_VERSION="0.1.5" \
  UPDATER_BASE_URL="https://example.invalid/releases/v0.1.5" \
  UPDATER_MANIFEST_OUTPUT="${tmp_root}/latest.json" \
  UPDATER_MACOS_AARCH64_ARTIFACT="${tmp_root}/Locality_Mac_Updater_v0.1.5.app.tar.gz" \
  UPDATER_LINUX_X86_64_ARTIFACT="${tmp_root}/Locality_Linux_v0.1.5.AppImage" \
  UPDATER_WINDOWS_X86_64_ARTIFACT="${tmp_root}/Locality_Windows_v0.1.5.exe" \
  "${UPDATER_SCRIPT}" >/dev/null

grep -F -q '"darwin-aarch64"' "${tmp_root}/latest.json" \
  || fail "updater manifest must accept explicit macOS platform artifacts without arch in the filename"
grep -F -q '"windows-x86_64"' "${tmp_root}/latest.json" \
  || fail "updater manifest must accept explicit Windows platform artifacts without arch in the filename"
grep -F -q '"linux-x86_64"' "${tmp_root}/latest.json" \
  || fail "updater manifest must accept explicit Linux platform artifacts without arch in the filename"
grep -F -q 'Locality_Mac_Updater_v0.1.5.app.tar.gz' "${tmp_root}/latest.json" \
  || fail "updater manifest must use the standard macOS updater filename"
grep -F -q 'Locality_Windows_v0.1.5.exe' "${tmp_root}/latest.json" \
  || fail "updater manifest must use the standard Windows installer filename"
grep -F -q 'Locality_Linux_v0.1.5.AppImage' "${tmp_root}/latest.json" \
  || fail "updater manifest must use the standard Linux AppImage filename"
