#!/usr/bin/env bash
set -euo pipefail

# Source and release metadata: https://www.kernel.org/pub/software/libs/libgpiod/
LIBGPIOD_VERSION=2.2.1
LIBGPIOD_SHA256=8f8f88f4ce764b02d03cc376f0a88cab028c63f94149e2cb5074301423f99098
LIBGPIOD_ARCHIVE="libgpiod-${LIBGPIOD_VERSION}.tar.gz"
LIBGPIOD_URL="https://www.kernel.org/pub/software/libs/libgpiod/${LIBGPIOD_ARCHIVE}"
PREFIX="${RUNNER_TEMP:-/tmp}/robot-hal-native"

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
test -f "$PREFIX/lib/libgpiod.so.3"
export LD_LIBRARY_PATH="$PREFIX/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
if [[ -n "${GITHUB_ENV:-}" ]]; then
  printf 'PKG_CONFIG_PATH=%s\n' "$PKG_CONFIG_PATH" >> "$GITHUB_ENV"
  printf 'LD_LIBRARY_PATH=%s\n' "$LD_LIBRARY_PATH" >> "$GITHUB_ENV"
fi
printf 'libgpiod=%s\n' "$(pkg-config --modversion libgpiod)"
printf 'libudev=%s\n' "$(pkg-config --modversion libudev)"
printf 'PKG_CONFIG_PATH=%s\n' "$PKG_CONFIG_PATH"
printf 'LD_LIBRARY_PATH=%s\n' "$LD_LIBRARY_PATH"
