#!/usr/bin/env bash

# Shared package-manager repository configuration. This file is sourced by the
# package builder and repository renderer so installed packages and hosted
# metadata cannot drift to different URLs or package names.

LINUX_REPOSITORY_BASE_URL="${LINUX_REPO_BASE_URL:-https://codeflash-ai.github.io/locality}"
LINUX_REPOSITORY_APT_SUITE="${APT_SUITE:-stable}"
LINUX_REPOSITORY_APT_COMPONENT="${APT_COMPONENT:-main}"
LINUX_REPOSITORY_APT_ARCH="${APT_ARCH:-amd64}"
LINUX_REPOSITORY_PACKAGE_NAME="${LINUX_REPO_PACKAGE_NAME:-locality}"
LINUX_REPOSITORY_APT_KEYRING="/usr/share/keyrings/codeflash-locality-archive-keyring.gpg"
LINUX_REPOSITORY_RPM_KEY="/etc/pki/rpm-gpg/RPM-GPG-KEY-codeflash-locality"

write_apt_repository_source() {
  local output="$1"
  cat > "${output}" <<EOF
Types: deb
URIs: ${LINUX_REPOSITORY_BASE_URL%/}/apt
Suites: ${LINUX_REPOSITORY_APT_SUITE}
Components: ${LINUX_REPOSITORY_APT_COMPONENT}
Architectures: ${LINUX_REPOSITORY_APT_ARCH}
Signed-By: ${LINUX_REPOSITORY_APT_KEYRING}
EOF
}

write_rpm_repository_config() {
  local output="$1"
  local gpg_key_location="$2"
  local repo_gpgcheck="${3:-1}"
  cat > "${output}" <<EOF
[locality]
name=Locality
baseurl=${LINUX_REPOSITORY_BASE_URL%/}/rpm/\$basearch
enabled=1
gpgcheck=0
repo_gpgcheck=${repo_gpgcheck}
gpgkey=${gpg_key_location}
EOF
}
