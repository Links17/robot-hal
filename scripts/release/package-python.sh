#!/usr/bin/env sh
set -eu

if [ "$#" -ne 2 ]; then
    printf '%s\n' "release.tool.invalid: expected tag output-dir" >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)

exec python3 "$script_dir/release_tool.py" package-python \
    --tag "$1" \
    --project "$repo_root/bindings/python" \
    --output-dir "$2"
