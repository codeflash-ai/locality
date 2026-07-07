#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
UPDATER_DIR="${ROOT}/target/release/bundle/updater"
OUTPUT="${UPDATER_MANIFEST_OUTPUT:-${UPDATER_DIR}/latest-macos.json}"
VERSION="${UPDATER_VERSION:-}"
BASE_URL="${UPDATER_BASE_URL:-${PUBLISH_RELEASE_BASE_URL:-}}"
RELEASE_TAG="${GITHUB_RELEASE_TAG:-${PUBLISH_RELEASE_TAG:-}}"
NOTES="${UPDATER_NOTES:-Locality desktop update.}"
PUB_DATE="${UPDATER_PUB_DATE:-$(date -u +"%Y-%m-%dT%H:%M:%SZ")}"

log() {
  printf 'updater-manifest: %s\n' "$*"
}

fail() {
  printf 'updater-manifest: error: %s\n' "$*" >&2
  exit 1
}

json_escape() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//$'\n'/\\n}"
  printf '%s' "${value}"
}

version_from_tauri_config() {
  sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
    "${ROOT}/apps/desktop/src-tauri/tauri.conf.json" | head -n 1
}

platform_for_archive() {
  local archive="$1"
  case "$(basename "${archive}")" in
    *.app.tar.gz)
      case "$(basename "${archive}")" in
        *aarch64*|*arm64*) printf 'darwin-aarch64\n' ;;
        *) fail "macOS updater artifact name must include aarch64 or arm64: ${archive}" ;;
      esac
      ;;
    *.AppImage)
      case "$(basename "${archive}")" in
        *x86_64*|*amd64*) printf 'linux-x86_64\n' ;;
        *aarch64*|*arm64*) printf 'linux-aarch64\n' ;;
        *) fail "Linux updater artifact name must include x86_64, amd64, aarch64, or arm64: ${archive}" ;;
      esac
      ;;
    *.nsis.zip|*.msi.zip|*.exe)
      case "$(basename "${archive}")" in
        *x86_64*|*amd64*) printf 'windows-x86_64\n' ;;
        *aarch64*|*arm64*) printf 'windows-aarch64\n' ;;
        *) fail "Windows updater artifact name must include x86_64, amd64, aarch64, or arm64: ${archive}" ;;
      esac
      ;;
    *)
      fail "unsupported updater artifact type: ${archive}"
      ;;
  esac
}

artifact_url() {
  local archive="$1"
  [[ -n "${BASE_URL}" ]] || fail "set UPDATER_BASE_URL, PUBLISH_RELEASE_BASE_URL, GITHUB_RELEASE_TAG, or PUBLISH_RELEASE_TAG"
  printf '%s/%s\n' "${BASE_URL%/}" "$(basename "${archive}")"
}

main() {
  VERSION="${VERSION:-$(version_from_tauri_config)}"
  [[ -n "${VERSION}" ]] || fail "set UPDATER_VERSION or define bundle.version in tauri.conf.json"
  if [[ -z "${BASE_URL}" && -n "${RELEASE_TAG}" ]]; then
    BASE_URL="https://github.com/codeflash-ai/locality/releases/download/${RELEASE_TAG}"
  fi

  local -a archives=()
  if [[ -n "${UPDATER_MACOS_AARCH64_ARTIFACT:-}" ]]; then
    archives+=("${UPDATER_MACOS_AARCH64_ARTIFACT}")
  fi
  if [[ -n "${UPDATER_LINUX_X86_64_ARTIFACT:-}" ]]; then
    archives+=("${UPDATER_LINUX_X86_64_ARTIFACT}")
  fi
  if [[ -n "${UPDATER_LINUX_AARCH64_ARTIFACT:-}" ]]; then
    archives+=("${UPDATER_LINUX_AARCH64_ARTIFACT}")
  fi
  if [[ -n "${UPDATER_WINDOWS_X86_64_ARTIFACT:-}" ]]; then
    archives+=("${UPDATER_WINDOWS_X86_64_ARTIFACT}")
  fi
  if [[ -n "${UPDATER_WINDOWS_AARCH64_ARTIFACT:-}" ]]; then
    archives+=("${UPDATER_WINDOWS_AARCH64_ARTIFACT}")
  fi
  if [[ "${#archives[@]}" == "0" ]]; then
    while IFS= read -r archive; do
      archives+=("${archive}")
    done < <(
      find "${UPDATER_DIR}" -maxdepth 1 -type f \
        \( -name '*.app.tar.gz' -o -name '*.AppImage' -o -name '*.nsis.zip' -o -name '*.msi.zip' -o -name '*.exe' \) | sort
    )
  fi
  [[ "${#archives[@]}" -gt 0 ]] || fail "no updater artifacts found in ${UPDATER_DIR}"

  mkdir -p "$(dirname "${OUTPUT}")"

  {
    printf '{\n'
    printf '  "version": "%s",\n' "$(json_escape "${VERSION}")"
    printf '  "notes": "%s",\n' "$(json_escape "${NOTES}")"
    printf '  "pub_date": "%s",\n' "$(json_escape "${PUB_DATE}")"
    printf '  "platforms": {\n'

    local first=1
    local archive platform signature url
    for archive in "${archives[@]}"; do
      [[ -f "${archive}" ]] || fail "missing updater artifact: ${archive}"
      [[ -f "${archive}.sig" ]] || fail "missing updater signature: ${archive}.sig"
      platform="$(platform_for_archive "${archive}")"
      signature="$(tr -d '\r\n' < "${archive}.sig")"
      url="$(artifact_url "${archive}")"
      if [[ "${first}" == "0" ]]; then
        printf ',\n'
      fi
      first=0
      printf '    "%s": {\n' "$(json_escape "${platform}")"
      printf '      "signature": "%s",\n' "$(json_escape "${signature}")"
      printf '      "url": "%s"\n' "$(json_escape "${url}")"
      printf '    }'
    done

    printf '\n  }\n'
    printf '}\n'
  } > "${OUTPUT}"

  log "wrote ${OUTPUT}"
}

main "$@"
