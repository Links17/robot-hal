param(
    [Parameter(Position = 0)][string]$Tag,
    [Parameter(Position = 1)][string]$ArtifactsDir,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Remaining
)

$ErrorActionPreference = "Stop"
if (
    [string]::IsNullOrEmpty($Tag) -or
    [string]::IsNullOrEmpty($ArtifactsDir) -or
    $Remaining.Count -ne 0
) {
    [Console]::Error.WriteLine("release.tool.invalid: expected tag complete-release-dir")
    exit 1
}

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir "../..")).Path
& python $ScriptDir/release_tool.py verify-artifacts `
    --tag $Tag `
    --artifacts-dir $ArtifactsDir `
    --targets (Join-Path $RepoRoot "release/targets.toml") `
    --repo-root $RepoRoot
exit $LASTEXITCODE
