#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INPUT_DIR="${LINUX_REPO_INPUT_DIR:-${ROOT}/target/release/bundle/linux}"
OUTPUT_DIR="${LINUX_REPO_OUTPUT_DIR:-${ROOT}/target/release/linux-repo}"
BASE_URL="${LINUX_REPO_BASE_URL:-https://codeflash-ai.github.io/locality}"
APT_SUITE="${APT_SUITE:-stable}"
APT_COMPONENT="${APT_COMPONENT:-main}"
APT_ARCH="${APT_ARCH:-amd64}"
RPM_ARCH="${RPM_ARCH:-x86_64}"
PACKAGE_PATTERN="${LINUX_REPO_PACKAGE_PATTERN:-Locality-release-[0-9]*-*}"
GPG_HOME=""

log() {
  printf 'linux-repo: %s\n' "$*"
}

fail() {
  printf 'linux-repo: error: %s\n' "$*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "missing required command: $1"
}

cleanup() {
  if [[ -n "${GPG_HOME}" ]]; then
    rm -rf "${GPG_HOME}"
  fi
}

gpg_signing_enabled() {
  [[ -n "${LINUX_REPO_GPG_PRIVATE_KEY:-}" || -n "${LINUX_REPO_GPG_KEY_ID:-}" ]]
}

setup_gpg() {
  if ! gpg_signing_enabled; then
    return 0
  fi

  require_command gpg
  if [[ -n "${LINUX_REPO_GPG_PRIVATE_KEY:-}" ]]; then
    GPG_HOME="$(mktemp -d)"
    chmod 700 "${GPG_HOME}"
    export GNUPGHOME="${GPG_HOME}"
    printf '%s\n' "${LINUX_REPO_GPG_PRIVATE_KEY}" | gpg --batch --import
  fi
}

gpg_key_id() {
  if [[ -n "${LINUX_REPO_GPG_KEY_ID:-}" ]]; then
    printf '%s\n' "${LINUX_REPO_GPG_KEY_ID}"
    return 0
  fi
  gpg --batch --list-secret-keys --with-colons \
    | awk -F: '$1 == "sec" { print $5; exit }'
}

gpg_args() {
  local key_id="$1"
  local -a args=(--batch --yes --local-user "${key_id}")
  if [[ -n "${LINUX_REPO_GPG_PASSPHRASE:-}" ]]; then
    args+=(--pinentry-mode loopback --passphrase "${LINUX_REPO_GPG_PASSPHRASE}")
  fi
  printf '%s\0' "${args[@]}"
}

sign_file() {
  local key_id="$1"
  local mode="$2"
  local input="$3"
  local output="$4"
  local -a args=()
  while IFS= read -r -d '' arg; do
    args+=("${arg}")
  done < <(gpg_args "${key_id}")

  case "${mode}" in
    clearsign) gpg "${args[@]}" --clearsign --output "${output}" "${input}" ;;
    detach) gpg "${args[@]}" --armor --detach-sign --output "${output}" "${input}" ;;
    *) fail "unknown GPG signature mode: ${mode}" ;;
  esac
}

copy_unique() {
  local pattern="$1"
  local output="$2"
  local copied=0
  while IFS= read -r artifact; do
    cp "${artifact}" "${output}/"
    copied=1
  done < <(find "${INPUT_DIR}" -maxdepth 1 -type f -name "${pattern}" | sort)
  [[ "${copied}" == "1" ]] || fail "no artifacts matched ${pattern} in ${INPUT_DIR}"
}

render_index() {
  local output="$1"
  cat > "${output}/index.html" <<EOF
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <title>Locality Linux Packages</title>
  </head>
  <body>
    <h1>Locality Linux Packages</h1>
    <ul>
      <li><a href="apt/">APT repository</a></li>
      <li><a href="rpm/">RPM repository</a></li>
    </ul>
  </body>
</html>
EOF
}

render_apt_repo() {
  require_command dpkg-scanpackages
  require_command apt-ftparchive
  require_command gzip

  local apt_root="${OUTPUT_DIR}/apt"
  local pool="${apt_root}/pool/main/a/loc"
  mkdir -p "${pool}" "${apt_root}/dists/${APT_SUITE}/${APT_COMPONENT}/binary-${APT_ARCH}"
  copy_unique "${PACKAGE_PATTERN}.deb" "${pool}"

  (
    cd "${apt_root}"
    dpkg-scanpackages --multiversion pool /dev/null > "dists/${APT_SUITE}/${APT_COMPONENT}/binary-${APT_ARCH}/Packages"
    gzip -kf "dists/${APT_SUITE}/${APT_COMPONENT}/binary-${APT_ARCH}/Packages"
    apt-ftparchive \
      -o "APT::FTPArchive::Release::Origin=CodeFlash" \
      -o "APT::FTPArchive::Release::Label=Locality" \
      -o "APT::FTPArchive::Release::Suite=${APT_SUITE}" \
      -o "APT::FTPArchive::Release::Codename=${APT_SUITE}" \
      -o "APT::FTPArchive::Release::Architectures=${APT_ARCH}" \
      -o "APT::FTPArchive::Release::Components=${APT_COMPONENT}" \
      release "dists/${APT_SUITE}" > "dists/${APT_SUITE}/Release"
  )
}

render_rpm_repo() {
  require_command createrepo_c

  local rpm_root="${OUTPUT_DIR}/rpm"
  mkdir -p "${rpm_root}/${RPM_ARCH}"
  copy_unique "${PACKAGE_PATTERN}.rpm" "${rpm_root}/${RPM_ARCH}"
  createrepo_c "${rpm_root}/${RPM_ARCH}"
}

write_rpm_repo_file() {
  local rpm_root="${OUTPUT_DIR}/rpm"
  local signed="$1"
  local repo_gpgcheck=0
  if [[ "${signed}" == "1" ]]; then
    repo_gpgcheck=1
  fi
  cat > "${rpm_root}/loc.repo" <<EOF
[loc]
name=Locality
baseurl=${BASE_URL%/}/rpm/${RPM_ARCH}
enabled=1
gpgcheck=0
repo_gpgcheck=${repo_gpgcheck}
gpgkey=${BASE_URL%/}/rpm/RPM-GPG-KEY-codeflash-loc
EOF
}

sign_repositories() {
  if ! gpg_signing_enabled; then
    write_rpm_repo_file 0
    log "GPG signing disabled; set LINUX_REPO_GPG_PRIVATE_KEY to publish signed repo metadata"
    return 0
  fi

  local key_id
  key_id="$(gpg_key_id)"
  [[ -n "${key_id}" ]] || fail "could not find a GPG signing key"

  local apt_release="${OUTPUT_DIR}/apt/dists/${APT_SUITE}/Release"
  sign_file "${key_id}" clearsign "${apt_release}" "${OUTPUT_DIR}/apt/dists/${APT_SUITE}/InRelease"
  sign_file "${key_id}" detach "${apt_release}" "${OUTPUT_DIR}/apt/dists/${APT_SUITE}/Release.gpg"
  gpg --batch --armor --export "${key_id}" > "${OUTPUT_DIR}/apt/codeflash-loc.asc"

  local repomd="${OUTPUT_DIR}/rpm/${RPM_ARCH}/repodata/repomd.xml"
  sign_file "${key_id}" detach "${repomd}" "${repomd}.asc"
  gpg --batch --armor --export "${key_id}" > "${OUTPUT_DIR}/rpm/RPM-GPG-KEY-codeflash-loc"
  write_rpm_repo_file 1
}

main() {
  trap cleanup EXIT
  require_command find
  require_command cp

  rm -rf "${OUTPUT_DIR}"
  mkdir -p "${OUTPUT_DIR}"
  setup_gpg
  render_index "${OUTPUT_DIR}"
  render_apt_repo
  render_rpm_repo
  sign_repositories
  log "wrote ${OUTPUT_DIR}"
}

main "$@"
