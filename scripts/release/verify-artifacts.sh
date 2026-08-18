#!/usr/bin/env sh
set -eu

if [ "$#" -ne 2 ]; then
    printf '%s\n' "release.tool.invalid: expected tag complete-release-dir" >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)

exec python3 "$script_dir/release_tool.py" verify-artifacts \
    --tag "$1" \
    --artifacts-dir "$2" \
    --targets "$repo_root/release/targets.toml" \
    --repo-root "$repo_root"
