#!/usr/bin/env sh
set -eu

if [ "$#" -ne 5 ]; then
    printf '%s\n' "release.tool.invalid: expected tag target binary manifest output-dir" >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)

exec python3 "$script_dir/release_tool.py" package-broker \
    --tag "$1" \
    --target "$2" \
    --binary "$3" \
    --manifest "$4" \
    --output-dir "$5" \
    --targets "$repo_root/release/targets.toml"
