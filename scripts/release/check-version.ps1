$ErrorActionPreference = "Stop"

if ($args.Count -lt 1) {
    throw "release tag required"
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "../..")).Path
python3 (Join-Path $repoRoot "scripts/release/release_tool.py") check-version `
    --tag $args[0] --repo-root $repoRoot
exit $LASTEXITCODE
