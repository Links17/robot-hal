#!/usr/bin/env sh
set -eu

if [ "$#" -ne 5 ]; then
    printf '%s\n' "release.tool.invalid: expected tag commit artifacts-dir software-uri hardware-uri" >&2
    exit 1
fi

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH= cd -- "$script_dir/../.." && pwd)

exec python3 "$script_dir/release_tool.py" generate-manifest \
    --tag "$1" \
    --commit "$2" \
    --artifacts-dir "$3" \
    --output-dir "$3" \
    --software-qualification "$4" \
    --hardware-qualification "$5"
