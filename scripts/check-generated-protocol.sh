#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
output_dir="$repo_root/bindings/python/seeed_hal/proto"
output_path="bindings/python/seeed_hal/proto"
temporary_root="$(mktemp -d "${TMPDIR:-/tmp}/seeed-hal-proto.XXXXXX")"
generated_dir="$temporary_root/proto"
trap 'rm -rf "$temporary_root"' EXIT

SEEED_HAL_PROTO_OUTPUT_DIR="$generated_dir" \
  "$repo_root/scripts/generate-protocol.sh"

failed=0
unexpected_generated="$({
  find "$generated_dir" -type f -name '*.py' \
    ! -path "$generated_dir/hal_pb2.py"
} | LC_ALL=C sort)"
if [[ ! -f "$generated_dir/hal_pb2.py" || -n "$unexpected_generated" ]]; then
  printf 'generator produced an unexpected file set:\n%s\n' \
    "$unexpected_generated" >&2
  failed=1
fi

unexpected_tracked="$({
  find "$output_dir" -type f -name '*.py' \
    ! -path "$output_dir/__init__.py" \
    ! -path "$output_dir/hal_pb2.py"
} | LC_ALL=C sort)"
if [[ ! -f "$output_dir/__init__.py" \
  || ! -f "$output_dir/hal_pb2.py" \
  || -n "$unexpected_tracked" ]]; then
  printf 'unexpected generated protocol output:\n%s\n' \
    "$unexpected_tracked" >&2
  failed=1
fi

if [[ -f "$generated_dir/hal_pb2.py" && -f "$output_dir/hal_pb2.py" ]] && \
  ! cmp -s "$generated_dir/hal_pb2.py" "$output_dir/hal_pb2.py"; then
  printf 'generated protocol binding differs from checked-in hal_pb2.py\n' >&2
  failed=1
fi

status="$(git -C "$repo_root" status --porcelain --untracked-files=all -- "$output_path")"
if [[ -n "$status" ]]; then
  printf 'generated protocol bindings are stale:\n%s\n' "$status" >&2
  failed=1
fi

exit "$failed"
