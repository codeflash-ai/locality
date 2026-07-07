#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=./clean-start-lib.sh
source "${ROOT}/scripts/clean-start-lib.sh"

usage() {
  cat <<'USAGE'
Usage: scripts/clean-start.sh [--yes] [--keep-credentials] [--state-dir PATH] [--app-path PATH]

Reset this machine to a fresh-install Locality testing state.

By default this is a dry run. Pass --yes to actually stop processes, unregister
File Provider domains, remove local Locality state, remove Locality app bundles
from standard install locations, remove Locality File Provider persistence, and
delete Locality keychain connection credentials.

Options:
  --yes               Execute the cleanup. Without this flag, only print actions.
  --keep-credentials  Do not delete Locality connection secrets from the keychain.
  --state-dir PATH    State directory to delete. Defaults to LOCALITY_STATE_DIR or ~/.loc.
  --app-path PATH     Additional Locality app bundle to delete.
  -h, --help          Show this help.
USAGE
}

DRY_RUN=1
KEEP_CREDENTIALS=0
STATE_DIR="${LOCALITY_STATE_DIR:-${HOME}/.loc}"
EXTRA_APP_PATH=""
DB_PATH=""
APP_PATHS=()
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"

log() {
  printf '%s\n' "$*"
}

warn() {
  printf 'clean-start: warning: %s\n' "$*" >&2
}

print_cmd() {
  printf '+'
  for arg in "$@"; do
    printf ' %q' "$arg"
  done
  printf '\n'
}

run() {
  print_cmd "$@"
  if [[ "${DRY_RUN}" -eq 0 ]]; then
    "$@" || warn "command failed: $*"
  fi
}

run_quiet() {
  print_cmd "$@"
  if [[ "${DRY_RUN}" -eq 0 ]]; then
    "$@" >/dev/null 2>&1 || true
  fi
}

prepare_path_for_removal() {
  local path="$1"
  [[ "${DRY_RUN}" -eq 0 ]] || return 0
  [[ "$(uname -s)" == "Darwin" ]] || return 0

  command -v chflags >/dev/null 2>&1 && chflags -R nouchg "${path}" >/dev/null 2>&1 || true
  command -v chmod >/dev/null 2>&1 && chmod -RN "${path}" >/dev/null 2>&1 || true
}

remove_path() {
  local path="$1"
  [[ -e "${path}" || -L "${path}" ]] || return 0
  prepare_path_for_removal "${path}"
  run rm -rf "${path}"
}

read_mount_roots() {
  [[ -f "${DB_PATH}" ]] || return 0
  command -v sqlite3 >/dev/null 2>&1 || return 0
  sqlite3 -noheader -cmd '.timeout 5000' "${DB_PATH}" \
    "select root from mounts where root is not null and root <> '';" 2>/dev/null || true
}

read_keychain_accounts() {
  [[ -f "${DB_PATH}" ]] || return 0
  command -v sqlite3 >/dev/null 2>&1 || return 0
  sqlite3 -noheader -cmd '.timeout 5000' "${DB_PATH}" \
    "select secret_ref from connections where secret_ref like 'connection:%';" 2>/dev/null || true
}

is_safe_mount_root_to_remove() {
  local path="$1"
  case "${path}" in
    "${HOME}/Documents/Locality"|\
    "${HOME}/Documents/Locality/"*|\
    "${HOME}/Library/CloudStorage/Locality"|\
    "${HOME}/Library/CloudStorage/Locality/"*|\
    "${HOME}/Library/CloudStorage/Locality-"*|\
    "${HOME}/Library/CloudStorage/Locality-"*|\
    /tmp/loc|\
    /tmp/loc/*|\
    /tmp/Locality|\
    /tmp/Locality/*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

unmount_if_mounted() {
  local path="$1"
  [[ -n "${path}" ]] || return 0
  if mount | grep -F " on ${path} (" >/dev/null 2>&1; then
    if [[ "$(uname -s)" == "Darwin" ]]; then
      run_quiet diskutil unmount force "${path}"
    fi
    run_quiet umount -f "${path}"
  fi
}

delete_keychain_account() {
  local account="$1"
  [[ -n "${account}" ]] || return 0
  if command -v security >/dev/null 2>&1; then
    run_quiet security delete-generic-password -s loc -a "${account}"
  fi
}

installed_helper() {
  local helper
  while IFS= read -r helper; do
    [[ -x "${helper}" ]] && {
      printf '%s\n' "${helper}"
      return 0
    }
  done < <(clean_start_target_helper_paths "${EXTRA_APP_PATH}")
}

repo_helper() {
  local helper="${ROOT}/apps/desktop/src-tauri/macos/LocalityFileProvider/locality-file-providerctl"
  [[ -x "${helper}" ]] && printf '%s\n' "${helper}"
}

remove_cli_link_if_locality_app_link() {
  local path="$1"
  local target app_path
  [[ -L "${path}" ]] || return 0

  target="$(readlink "${path}" 2>/dev/null || true)"
  for app_path in "${APP_PATHS[@]}"; do
    case "${target}" in
      "${app_path}/Contents/MacOS/loc"|*/Locality.app/Contents/MacOS/loc)
        run rm -f "${path}"
        return 0
        ;;
    esac
  done
}

reset_file_provider_domains() {
  [[ "$(uname -s)" == "Darwin" ]] || return 0

  local helper=""
  helper="$(installed_helper || true)"
  if [[ -z "${helper}" ]]; then
    helper="$(repo_helper || true)"
  fi

  if [[ -n "${helper}" ]]; then
    run_quiet "${helper}" reset --json
  else
    warn "locality-file-providerctl not found; skipping File Provider domain reset"
  fi
}

stop_processes() {
  local app_path
  if [[ "$(uname -s)" == "Darwin" ]]; then
    run_quiet osascript -e 'tell application id "ai.codeflash.locality" to quit'
    run_quiet launchctl bootout "gui/${UID}/ai.codeflash.locality.desktop"
    run_quiet launchctl bootout "gui/${UID}/ai.codeflash.locality.localityd"
    run_quiet launchctl bootout "gui/${UID}" "${HOME}/Library/LaunchAgents/ai.codeflash.locality.desktop.plist"
    run_quiet launchctl bootout "gui/${UID}" "${HOME}/Library/LaunchAgents/ai.codeflash.locality.localityd.plist"
  fi

  run_quiet pkill -x locality-desktop
  run_quiet pkill -x Locality
  run_quiet pkill -x LocalityFileProvider
  run_quiet pkill -x localityd
  run_quiet pkill -f "${ROOT}/target/.*/locality-desktop"
  run_quiet pkill -f "${ROOT}/target/.*/localityd"
  for app_path in "${APP_PATHS[@]}"; do
    run_quiet pkill -f "${app_path}/Contents"
  done
}

remove_mount_roots() {
  local roots=()
  local root

  while IFS= read -r root; do
    [[ -n "${root}" ]] && append_unique roots "${root}"
  done < <(read_mount_roots)

  while IFS= read -r root; do
    [[ -n "${root}" ]] && append_unique roots "${root}"
  done < <(clean_start_mount_root_candidates)

  for root in "${roots[@]}"; do
    unmount_if_mounted "${root}"
    if is_safe_mount_root_to_remove "${root}"; then
      remove_path "${root}"
    elif [[ -e "${root}" || -L "${root}" ]]; then
      warn "not removing non-standard mount root: ${root}"
    fi
  done
}

remove_credentials() {
  [[ "${KEEP_CREDENTIALS}" -eq 0 ]] || return 0

  local accounts=()
  local account
  while IFS= read -r account; do
    [[ -n "${account}" ]] && accounts+=("${account}")
  done < <(read_keychain_accounts)

  accounts+=(
    "connection:notion-default"
    "connection:notion-main"
    "connection:notion-test"
  )

  local unique_accounts=()
  for account in "${accounts[@]}"; do
    append_unique unique_accounts "${account}"
  done

  for account in "${unique_accounts[@]}"; do
    delete_keychain_account "${account}"
  done
}

remove_support_files() {
  local support_path
  if [[ "$(uname -s)" == "Darwin" ]]; then
    while IFS= read -r support_path; do
      [[ -n "${support_path}" ]] || continue
      remove_path "${support_path}"
    done < <(clean_start_support_paths)
    remove_cli_link_if_locality_app_link "${HOME}/.local/bin/loc"
    remove_cli_link_if_locality_app_link "${HOME}/bin/loc"
    remove_cli_link_if_locality_app_link "/opt/homebrew/bin/loc"
    remove_cli_link_if_locality_app_link "/usr/local/bin/loc"
  fi
  remove_path "${STATE_DIR}"
}

remove_apps() {
  local app_path plugin_path
  if [[ "$(uname -s)" == "Darwin" ]]; then
    while IFS= read -r plugin_path; do
      [[ -n "${plugin_path}" ]] || continue
      run_quiet pluginkit -r "${plugin_path}"
    done < <(clean_start_target_plugin_paths "${EXTRA_APP_PATH}")
  fi
  for app_path in "${APP_PATHS[@]}"; do
    if [[ "$(uname -s)" == "Darwin" ]] && [[ -x "${LSREGISTER}" ]]; then
      run_quiet "${LSREGISTER}" -u "${app_path}"
    fi
    remove_path "${app_path}"
  done
}

clean_start_main() {
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --yes)
        DRY_RUN=0
        shift
        ;;
      --keep-credentials)
        KEEP_CREDENTIALS=1
        shift
        ;;
      --state-dir)
        STATE_DIR="${2:?--state-dir requires a path}"
        shift 2
        ;;
      --app-path)
        EXTRA_APP_PATH="${2:?--app-path requires a path}"
        shift 2
        ;;
      -h|--help)
        usage
        return 0
        ;;
      *)
        echo "clean-start: unknown argument: $1" >&2
        usage >&2
        return 2
        ;;
    esac
  done

  STATE_DIR="${STATE_DIR/#\~/${HOME}}"
  EXTRA_APP_PATH="${EXTRA_APP_PATH/#\~/${HOME}}"
  DB_PATH="${STATE_DIR}/state.sqlite3"

  APP_PATHS=()
  while IFS= read -r app_path; do
    [[ -n "${app_path}" ]] || continue
    APP_PATHS+=("${app_path}")
  done < <(clean_start_target_app_paths "${EXTRA_APP_PATH}")

  log "Locality clean-start"
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "Mode: dry run. Re-run with --yes to execute."
  else
    log "Mode: executing cleanup."
  fi
  log "State dir: ${STATE_DIR}"
  for app_path in "${APP_PATHS[@]}"; do
    log "App path:  ${app_path}"
  done
  if [[ "${KEEP_CREDENTIALS}" -eq 1 ]]; then
    log "Credentials: preserving keychain entries."
  else
    log "Credentials: deleting Locality connection keychain entries."
  fi
  log ""

  stop_processes
  reset_file_provider_domains
  remove_mount_roots
  remove_credentials
  remove_apps
  remove_support_files

  log ""
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "Dry run complete. No changes were made."
  else
    log "Clean-start reset complete."
  fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  clean_start_main "$@"
fi
