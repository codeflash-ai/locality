#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DESKTOP_DIR="${ROOT}/apps/desktop"
DEB_DIR="${ROOT}/target/release/bundle/deb"
RPM_DIR="${ROOT}/target/release/bundle/rpm"
APPIMAGE_DIR="${ROOT}/target/release/bundle/appimage"
LINUX_OUT_DIR="${ROOT}/target/release/bundle/linux"
UPDATER_DIR="${ROOT}/target/release/bundle/updater"
PRODUCT_NAME="${PUBLISH_PRODUCT_NAME:-Locality}"
CHANNEL="${PUBLISH_CHANNEL:-beta}"
DATE_STAMP="${PUBLISH_DATE:-$(date +%Y%m%d)}"
UPDATER_ENDPOINT="${TAURI_UPDATER_ENDPOINT:-https://github.com/codeflash-ai/locality/releases/latest/download/latest-linux.json}"
APPINDICATOR_PKG_CONFIG_TMP=""

log() {
  printf 'publish-linux: %s\n' "$*"
}

fail() {
  printf 'publish-linux: error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

json_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  printf '%s' "${value}"
}

cleanup_appindicator_pkg_config() {
  if [[ -n "${APPINDICATOR_PKG_CONFIG_TMP}" ]]; then
    rm -rf "${APPINDICATOR_PKG_CONFIG_TMP}"
  fi
}

assert_clean_tree() {
  if [[ "${PUBLISH_ALLOW_DIRTY:-0}" == "1" ]]; then
    return 0
  fi
  if [[ -n "$(git -C "${ROOT}" status --porcelain)" ]]; then
    fail "working tree has uncommitted changes; commit them first or set PUBLISH_ALLOW_DIRTY=1"
  fi
}

latest_artifact() {
  local dir="$1"
  local pattern="$2"
  find "${dir}" -maxdepth 1 -type f -name "${pattern}" | sort | tail -n 1
}

updater_enabled() {
  [[ -n "${TAURI_UPDATER_PUBKEY:-}" ]]
}

build_config_json() {
  if updater_enabled; then
    [[ -n "${TAURI_SIGNING_PRIVATE_KEY:-}" ]] \
      || fail "TAURI_UPDATER_PUBKEY is set but TAURI_SIGNING_PRIVATE_KEY is missing"
    printf '{"bundle":{"createUpdaterArtifacts":true},"plugins":{"updater":{"pubkey":"%s","endpoints":["%s"]}}}' \
      "$(json_escape "${TAURI_UPDATER_PUBKEY}")" \
      "$(json_escape "${UPDATER_ENDPOINT}")"
    return 0
  fi

  printf '{}'
}

pkg_config_has_appindicator() {
  pkg-config --exists ayatana-appindicator3-0.1 2>/dev/null \
    || pkg-config --exists appindicator3-0.1 2>/dev/null
}

ldconfig_library_path() {
  local library="$1"
  ldconfig -p 2>/dev/null | awk -v library="${library}" '$1 == library { print $NF; exit }'
}

write_appindicator_pc() {
  local pc_name="$1"
  local library_path="$2"
  local libdir
  libdir="$(dirname "${library_path}")"
  APPINDICATOR_PKG_CONFIG_TMP="$(mktemp -d)"
  cat >"${APPINDICATOR_PKG_CONFIG_TMP}/${pc_name}.pc" <<EOF
prefix=/usr
exec_prefix=\${prefix}
libdir=${libdir}

Name: ${pc_name}
Description: Tauri Linux package appindicator detection shim
Version: 0.1
Libs: -L\${libdir} ${library_path}
EOF
  export PKG_CONFIG_PATH="${APPINDICATOR_PKG_CONFIG_TMP}${PKG_CONFIG_PATH:+:${PKG_CONFIG_PATH}}"
  log "using temporary pkg-config metadata for ${library_path}"
}

prepare_appindicator_pkg_config() {
  if pkg_config_has_appindicator; then
    return 0
  fi

  local library_path
  library_path="$(ldconfig_library_path libayatana-appindicator3.so.1)"
  if [[ -n "${library_path}" ]]; then
    write_appindicator_pc "ayatana-appindicator3-0.1" "${library_path}"
    return 0
  fi

  library_path="$(ldconfig_library_path libappindicator3.so.1)"
  if [[ -n "${library_path}" ]]; then
    write_appindicator_pc "appindicator3-0.1" "${library_path}"
    return 0
  fi

  fail "Tauri Linux packaging needs libayatana-appindicator3 or libappindicator3 for the tray icon; install the distro package that provides ayatana-appindicator3-0.1.pc or appindicator3-0.1.pc"
}

deb_data_member() {
  ar t "$1" | grep -E '^data\.tar\.(gz|xz|zst)$|^data\.tar$' | head -n 1
}

deb_data_listing() {
  local deb="$1"
  local member
  member="$(deb_data_member "${deb}")"
  [[ -n "${member}" ]] || fail "${deb} does not contain a data.tar member"

  case "${member}" in
    *.tar.gz) ar p "${deb}" "${member}" | tar -tzf - ;;
    *.tar.xz) ar p "${deb}" "${member}" | tar -tJf - ;;
    *.tar.zst) ar p "${deb}" "${member}" | zstd -dc | tar -tf - ;;
    *.tar) ar p "${deb}" "${member}" | tar -tf - ;;
    *) fail "unsupported Debian data archive: ${member}" ;;
  esac
}

assert_deb_contains() {
  local deb="$1"
  local path="$2"
  local normalized_path="${path#/}"
  deb_data_listing "${deb}" | sed 's#^\./##' | grep -qx "${normalized_path}" \
    || fail "${deb} does not contain ${path}"
}

validate_deb() {
  local deb="$1"
  ar t "${deb}" | grep -qx 'debian-binary' || fail "${deb} is missing debian-binary"
  ar t "${deb}" | grep -Eq '^control\.tar\.(gz|xz|zst)$|^control\.tar$' \
    || fail "${deb} is missing control.tar"
  assert_deb_contains "${deb}" "/usr/bin/loc"
  assert_deb_contains "${deb}" "/usr/bin/localityd"
  assert_deb_contains "${deb}" "/usr/bin/locality-fuse"
}

validate_rpm() {
  local rpm="$1"
  rpm -qip "${rpm}" >/dev/null
  rpm -qlp "${rpm}" | grep -qx '/usr/bin/loc' \
    || fail "${rpm} does not contain /usr/bin/loc"
  rpm -qlp "${rpm}" | grep -qx '/usr/bin/localityd' \
    || fail "${rpm} does not contain /usr/bin/localityd"
  rpm -qlp "${rpm}" | grep -qx '/usr/bin/locality-fuse' \
    || fail "${rpm} does not contain /usr/bin/locality-fuse"
}

copy_artifact() {
  local src="$1"
  local ext="$2"
  local commit_short="$3"
  local arch="$4"
  local name dest sha

  name="${PRODUCT_NAME}-${CHANNEL}-${DATE_STAMP}-${commit_short}-${arch}.${ext}"
  dest="${LINUX_OUT_DIR}/${name}"
  cp "${src}" "${dest}"
  sha="$(sha256sum "${dest}" | awk '{print $1}')"
  printf '%s %s\n' "${sha}" "${dest}" > "${dest}.sha256"
  printf '%s\n' "${dest}"
}

copy_latest_alias() {
  local src="$1"
  local ext="$2"
  local arch="$3"
  local name dest sha

  name="${PRODUCT_NAME}-${CHANNEL}-linux-${arch}.${ext}"
  dest="${LINUX_OUT_DIR}/${name}"
  cp "${src}" "${dest}"
  sha="$(sha256sum "${dest}" | awk '{print $1}')"
  printf '%s %s\n' "${sha}" "${dest}" > "${dest}.sha256"
  printf '%s\n' "${dest}"
}

copy_updater_artifact() {
  local src="$1"
  local commit_short="$2"
  local arch="$3"
  local name dest alias

  [[ -f "${src}.sig" ]] || fail "Tauri did not produce ${src}.sig"

  name="${PRODUCT_NAME}-${CHANNEL}-${DATE_STAMP}-${commit_short}-linux-${arch}.AppImage"
  dest="${UPDATER_DIR}/${name}"
  alias="${UPDATER_DIR}/${PRODUCT_NAME}-${CHANNEL}-linux-${arch}.AppImage"
  mkdir -p "${UPDATER_DIR}"
  cp "${src}" "${dest}"
  cp "${src}.sig" "${dest}.sig"
  cp "${src}" "${alias}"
  cp "${src}.sig" "${alias}.sig"
  printf '%s\n' "${dest}"
}

main() {
  trap cleanup_appindicator_pkg_config EXIT
  [[ "$(uname -s)" == "Linux" ]] || fail "Linux publishing must run on Linux"
  require_command git
  require_command npm
  require_command cargo
  require_command ar
  require_command tar
  require_command zstd
  require_command rpm
  require_command sha256sum
  require_command pkg-config
  require_command ldconfig

  assert_clean_tree
  prepare_appindicator_pkg_config

  local commit_short commit_full config_json deb rpm appimage arch
  local final_deb final_rpm alias_deb alias_rpm updater_appimage
  commit_short="$(git -C "${ROOT}" rev-parse --short=7 HEAD)"
  commit_full="$(git -C "${ROOT}" rev-parse --short=12 HEAD)"
  arch="$(uname -m)"
  config_json="$(build_config_json)"

  log "commit ${commit_full}"
  if updater_enabled; then
    log "updater endpoint: ${UPDATER_ENDPOINT}"
  else
    log "Linux AppImage updater artifacts disabled; set TAURI_UPDATER_PUBKEY and TAURI_SIGNING_PRIVATE_KEY to enable"
  fi
  log "building Tauri Debian, RPM, and optional AppImage packages"
  rm -rf "${DEB_DIR}" "${RPM_DIR}" "${APPIMAGE_DIR}" "${UPDATER_DIR}"
  mkdir -p "${DEB_DIR}" "${RPM_DIR}" "${APPIMAGE_DIR}" "${LINUX_OUT_DIR}"
  if updater_enabled; then
    npm --prefix "${DESKTOP_DIR}" run tauri -- build --bundles deb,rpm,appimage --config "${config_json}"
  else
    npm --prefix "${DESKTOP_DIR}" run build:linux
  fi

  deb="$(latest_artifact "${DEB_DIR}" '*.deb')"
  rpm="$(latest_artifact "${RPM_DIR}" '*.rpm')"
  [[ -n "${deb}" && -f "${deb}" ]] || fail "Tauri did not produce a .deb artifact"
  [[ -n "${rpm}" && -f "${rpm}" ]] || fail "Tauri did not produce a .rpm artifact"

  log "validating Debian package"
  validate_deb "${deb}"
  log "validating RPM package"
  validate_rpm "${rpm}"

  final_deb="$(copy_artifact "${deb}" "deb" "${commit_short}" "${arch}")"
  final_rpm="$(copy_artifact "${rpm}" "rpm" "${commit_short}" "${arch}")"
  alias_deb="$(copy_latest_alias "${deb}" "deb" "${arch}")"
  alias_rpm="$(copy_latest_alias "${rpm}" "rpm" "${arch}")"

  if updater_enabled; then
    appimage="$(latest_artifact "${APPIMAGE_DIR}" '*.AppImage')"
    [[ -n "${appimage}" && -f "${appimage}" ]] || fail "Tauri did not produce an AppImage artifact"
    updater_appimage="$(copy_updater_artifact "${appimage}" "${commit_short}" "${arch}")"
  fi

  printf '\nPublished Linux packages:\n'
  printf '  %s\n' "${final_deb}"
  printf '  %s.sha256\n' "${final_deb}"
  printf '  %s\n' "${final_rpm}"
  printf '  %s.sha256\n' "${final_rpm}"
  printf '  %s\n' "${alias_deb}"
  printf '  %s.sha256\n' "${alias_deb}"
  printf '  %s\n' "${alias_rpm}"
  printf '  %s.sha256\n' "${alias_rpm}"
  if [[ -n "${updater_appimage:-}" ]]; then
    printf '  %s\n' "${updater_appimage}"
    printf '  %s.sig\n' "${updater_appimage}"
  fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  main "$@"
fi
