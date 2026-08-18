#!/usr/bin/env bash
set -euo pipefail

# Source and release metadata: https://www.kernel.org/pub/software/libs/libgpiod/
LIBGPIOD_VERSION=2.2.1
LIBGPIOD_SHA256=95689033324c16a13c32e947b9933553258544d6538466b04859a5d1ba950798
LIBGPIOD_ARCHIVE="libgpiod-${LIBGPIOD_VERSION}.tar.gz"
LIBGPIOD_URL="https://www.kernel.org/pub/software/libs/libgpiod/${LIBGPIOD_ARCHIVE}"
PREFIX="${RUNNER_TEMP:-/tmp}/seeed-hal-native"

export DEBIAN_FRONTEND=noninteractive
APT_OPTIONS=(
  -o Acquire::Retries=3
  -o Acquire::http::Timeout=30
  -o Acquire::https::Timeout=30
)

sudo apt-get "${APT_OPTIONS[@]}" update
sudo apt-get "${APT_OPTIONS[@]}" install --yes --no-install-recommends \
  build-essential \
  ca-certificates \
  curl \
  libudev-dev \
  pkg-config

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT
archive="$workdir/$LIBGPIOD_ARCHIVE"

curl --fail --location --retry 3 --retry-delay 2 --connect-timeout 30 \
  --max-time 300 --output "$archive" "$LIBGPIOD_URL"
printf '%s  %s\n' "$LIBGPIOD_SHA256" "$archive" | sha256sum --check

tar -xzf "$archive" -C "$workdir"
(
  cd "$workdir/libgpiod-${LIBGPIOD_VERSION}"
  ./configure --prefix="$PREFIX" --libdir="$PREFIX/lib" \
    --enable-tools=no --enable-bindings-python=no
  make -j"$(nproc)"
  make install
)

export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig"
pkg-config --exists 'libgpiod >= 2'
pkg-config --exists libudev
if [[ -n "${GITHUB_ENV:-}" ]]; then
  printf 'PKG_CONFIG_PATH=%s\n' "$PKG_CONFIG_PATH" >> "$GITHUB_ENV"
fi
printf 'libgpiod=%s\n' "$(pkg-config --modversion libgpiod)"
printf 'libudev=%s\n' "$(pkg-config --modversion libudev)"
printf 'PKG_CONFIG_PATH=%s\n' "$PKG_CONFIG_PATH"
