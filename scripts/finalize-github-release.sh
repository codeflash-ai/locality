#!/usr/bin/env bash
set -euo pipefail

RELEASE_TAG="${RELEASE_TAG:-${GITHUB_REF_NAME:-}}"
GITHUB_REPOSITORY="${GITHUB_REPOSITORY:-}"

if [[ -z "${GITHUB_REPOSITORY}" ]]; then
  echo "GITHUB_REPOSITORY is required." >&2
  exit 2
fi
if [[ -z "${RELEASE_TAG}" || "${RELEASE_TAG}" != v* ]]; then
  echo "RELEASE_TAG must be a v* release tag." >&2
  exit 2
fi

APP_VERSION="${RELEASE_TAG#v}"
if [[ "${APP_VERSION}" == *-* ]]; then
  echo "${RELEASE_TAG} looks like a prerelease tag; leaving it non-latest."
  exit 0
fi

required_workflows=(
  "release macOS"
  "release Linux"
  "release Windows"
)

for workflow in "${required_workflows[@]}"; do
  runs_json="$(gh run list \
    --repo "${GITHUB_REPOSITORY}" \
    --workflow "${workflow}" \
    --branch "${RELEASE_TAG}" \
    --limit 1 \
    --json status,conclusion,url)"

  if [[ "$(jq 'length' <<<"${runs_json}")" -eq 0 ]]; then
    echo "Waiting for ${workflow}: no run found for ${RELEASE_TAG} yet."
    exit 0
  fi

  status="$(jq -r '.[0].status' <<<"${runs_json}")"
  conclusion="$(jq -r '.[0].conclusion // ""' <<<"${runs_json}")"
  url="$(jq -r '.[0].url' <<<"${runs_json}")"

  if [[ "${status}" != "completed" ]]; then
    echo "Waiting for ${workflow}: ${status} (${url})."
    exit 0
  fi
  if [[ "${conclusion}" != "success" ]]; then
    echo "${workflow} completed with ${conclusion}: ${url}" >&2
    exit 1
  fi
done

release_json="$(gh release view "${RELEASE_TAG}" \
  --repo "${GITHUB_REPOSITORY}" \
  --json assets,isDraft,isPrerelease)"

if [[ "$(jq -r '.isDraft' <<<"${release_json}")" == "true" ]]; then
  echo "Release ${RELEASE_TAG} is still a draft; not promoting."
  exit 0
fi

required_assets=(
  "Locality_Mac_v${APP_VERSION}.dmg"
  "Locality_Mac.dmg"
  "Locality_Mac_Updater_v${APP_VERSION}.app.tar.gz"
  "Locality_Mac_Updater_v${APP_VERSION}.app.tar.gz.sig"
  "latest-macos.json"
  "SHA256SUMS"
  "loc.rb"
  "Locality_Linux_v${APP_VERSION}.deb"
  "Locality_Linux_v${APP_VERSION}.deb.sha256"
  "Locality_Linux_v${APP_VERSION}.rpm"
  "Locality_Linux_v${APP_VERSION}.rpm.sha256"
  "Locality_Linux_v${APP_VERSION}.AppImage"
  "Locality_Linux_v${APP_VERSION}.AppImage.sig"
  "Locality_Linux_v${APP_VERSION}.AppImage.sha256"
  "Locality_Linux.deb"
  "Locality_Linux.deb.sha256"
  "Locality_Linux.rpm"
  "Locality_Linux.rpm.sha256"
  "Locality_Linux.AppImage"
  "Locality_Linux.AppImage.sig"
  "Locality_Linux.AppImage.sha256"
  "latest-linux.json"
  "SHA256SUMS-linux"
  "Locality_Windows_v${APP_VERSION}.exe"
  "Locality_Windows_v${APP_VERSION}.exe.sha256"
  "Locality_Windows_v${APP_VERSION}.exe.sig"
  "Locality_Windows.exe"
  "Locality_Windows.exe.sha256"
  "Locality_Windows.exe.sig"
  "latest-windows.json"
  "SHA256SUMS-windows"
)

missing=()
for asset in "${required_assets[@]}"; do
  if ! jq -e --arg name "${asset}" 'any(.assets[]; .name == $name)' <<<"${release_json}" >/dev/null; then
    missing+=("${asset}")
  fi
done

if [[ "${#missing[@]}" -gt 0 ]]; then
  echo "Release ${RELEASE_TAG} is not complete; missing assets:"
  printf '  %s\n' "${missing[@]}"
  exit 0
fi

gh release edit "${RELEASE_TAG}" \
  --repo "${GITHUB_REPOSITORY}" \
  --prerelease=false \
  --latest=true

echo "Promoted ${RELEASE_TAG} to latest."
