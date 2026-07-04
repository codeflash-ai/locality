#!/usr/bin/env bash

LOCALITY_FILE_PROVIDER_BUNDLE_ID="ai.codeflash.locality.Locality.FileProvider"

append_unique() {
  local array_name="$1"
  local value="$2"
  local existing
  eval "set -- \"\${${array_name}[@]:-}\""
  for existing in "$@"; do
    [[ -n "${existing}" ]] || continue
    [[ "${existing}" == "${value}" ]] && return 0
  done
  eval "${array_name}+=(\"\${value}\")"
}

clean_start_target_app_paths() {
  local extra_app_path="${1:-}"
  local app_paths=()

  append_unique app_paths "/Applications/Locality.app"
  append_unique app_paths "${HOME}/Applications/Locality.app"

  if [[ -n "${extra_app_path}" ]]; then
    extra_app_path="${extra_app_path/#\~/${HOME}}"
    append_unique app_paths "${extra_app_path}"
  fi

  printf '%s\n' "${app_paths[@]}"
}

clean_start_target_helper_paths() {
  local extra_app_path="${1:-}"
  local helper_paths=()
  local app_path
  while IFS= read -r app_path; do
    [[ -n "${app_path}" ]] || continue
    append_unique helper_paths "${app_path}/Contents/MacOS/locality-file-providerctl"
  done < <(clean_start_target_app_paths "${extra_app_path}")
  printf '%s\n' "${helper_paths[@]}"
}

clean_start_registered_plugin_paths_from_match_output() {
  local plugin_paths=()
  local line path
  while IFS= read -r line; do
    case "${line}" in
      *"/"*"LocalityFileProvider.appex")
        path="/${line#*/}"
        append_unique plugin_paths "${path}"
        ;;
    esac
  done
  [[ ${#plugin_paths[@]} -gt 0 ]] || return 0
  printf '%s\n' "${plugin_paths[@]}"
}

clean_start_registered_plugin_paths() {
  [[ "$(uname -s)" == "Darwin" ]] || return 0
  command -v pluginkit >/dev/null 2>&1 || return 0
  pluginkit -m -D -v -i "${LOCALITY_FILE_PROVIDER_BUNDLE_ID}" 2>/dev/null \
    | clean_start_registered_plugin_paths_from_match_output
}

clean_start_target_plugin_paths() {
  local extra_app_path="${1:-}"
  local plugin_paths=()
  local app_path plugin_path
  while IFS= read -r app_path; do
    [[ -n "${app_path}" ]] || continue
    append_unique plugin_paths "${app_path}/Contents/PlugIns/LocalityFileProvider.appex"
  done < <(clean_start_target_app_paths "${extra_app_path}")
  while IFS= read -r plugin_path; do
    [[ -n "${plugin_path}" ]] || continue
    append_unique plugin_paths "${plugin_path}"
  done < <(clean_start_registered_plugin_paths)
  printf '%s\n' "${plugin_paths[@]}"
}

clean_start_mount_root_candidates() {
  local roots=(
    "${HOME}/Documents/Locality"
    "${HOME}/Library/CloudStorage/Locality"
  )
  local cloud_root
  for cloud_root in "${HOME}/Library/CloudStorage"/Locality-*; do
    [[ -e "${cloud_root}" ]] || continue
    append_unique roots "${cloud_root}"
  done
  printf '%s\n' "${roots[@]}"
}

clean_start_support_paths() {
  printf '%s\n' \
    "${HOME}/Library/LaunchAgents/ai.codeflash.locality.desktop.plist" \
    "${HOME}/Library/LaunchAgents/ai.codeflash.locality.localityd.plist" \
    "${HOME}/Library/Group Containers/C484HB7Q6S.group.ai.codeflash.locality" \
    "${HOME}/Library/Group Containers/group.ai.codeflash.locality" \
    "${HOME}/Library/Application Scripts/C484HB7Q6S.group.ai.codeflash.locality" \
    "${HOME}/Library/Application Scripts/group.ai.codeflash.locality" \
    "${HOME}/Library/Application Scripts/ai.codeflash.locality.Locality.FileProvider" \
    "${HOME}/Library/Application Scripts/ai.codeflash.locality.Locality.file-providerctl" \
    "${HOME}/Library/Application Support/ai.codeflash.locality" \
    "${HOME}/Library/Application Support/FileProvider/ai.codeflash.locality.Locality.FileProvider" \
    "${HOME}/Library/Caches/ai.codeflash.locality" \
    "${HOME}/Library/Containers/ai.codeflash.locality.Locality.FileProvider" \
    "${HOME}/Library/Containers/ai.codeflash.locality.Locality.file-providerctl" \
    "${HOME}/Library/HTTPStorages/ai.codeflash.locality" \
    "${HOME}/Library/Preferences/ai.codeflash.locality.plist" \
    "${HOME}/Library/Saved Application State/ai.codeflash.locality.savedState" \
    "${HOME}/Library/WebKit/ai.codeflash.locality"
}
