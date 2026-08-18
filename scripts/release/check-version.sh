#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
python3 "$repo_root/scripts/release/release_tool.py" check-version \
  --tag "${1:?release tag required}" --repo-root "$repo_root"
