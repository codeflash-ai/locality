#!/usr/bin/env bash

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
  local app_path
  while IFS= read -r app_path; do
    [[ -n "${app_path}" ]] || continue
    printf '%s\n' "${app_path}/Contents/MacOS/locality-file-providerctl"
  done < <(clean_start_target_app_paths "${extra_app_path}")
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
