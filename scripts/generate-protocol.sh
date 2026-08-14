#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
python_project="$repo_root/bindings/python"
proto_dir="$repo_root/proto/seeed/hal/v1"
output_dir="$python_project/seeed_hal/proto"

mkdir -p "$output_dir"
uv run --frozen --project "$python_project" python -m grpc_tools.protoc \
  --proto_path="$proto_dir" \
  --python_out="$output_dir" \
  "$proto_dir/hal.proto"
