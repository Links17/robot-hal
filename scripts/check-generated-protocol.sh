#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_dir="$repo_root/bindings/python/seeed_hal/proto"
output_path="bindings/python/seeed_hal/proto"

"$repo_root/scripts/generate-protocol.sh"

unexpected="$({
  find "$output_dir" -type f -name '*.py' \
    ! -path "$output_dir/__init__.py" \
    ! -path "$output_dir/hal_pb2.py"
} | LC_ALL=C sort)"
if [[ -n "$unexpected" ]]; then
  printf 'unexpected generated protocol output:\n%s\n' "$unexpected" >&2
  exit 1
fi

status="$(git -C "$repo_root" status --porcelain --untracked-files=all -- "$output_path")"
if [[ -n "$status" ]]; then
  printf 'generated protocol bindings are stale:\n%s\n' "$status" >&2
  exit 1
fi
