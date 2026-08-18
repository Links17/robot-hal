param(
    [Parameter(Position = 0)][string]$Tag,
    [Parameter(Position = 1)][string]$Target,
    [Parameter(Position = 2)][string]$Binary,
    [Parameter(Position = 3)][string]$Manifest,
    [Parameter(Position = 4)][string]$OutputDir,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Remaining
)

$ErrorActionPreference = "Stop"
if (
    [string]::IsNullOrEmpty($Tag) -or
    [string]::IsNullOrEmpty($Target) -or
    [string]::IsNullOrEmpty($Binary) -or
    [string]::IsNullOrEmpty($Manifest) -or
    [string]::IsNullOrEmpty($OutputDir) -or
    $Remaining.Count -ne 0
) {
    [Console]::Error.WriteLine(
        "release.tool.invalid: expected tag target binary manifest output-dir"
    )
    exit 1
}

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir "../..")).Path

& python $ScriptDir/release_tool.py package-broker `
    --tag $Tag `
    --target $Target `
    --binary $Binary `
    --manifest $Manifest `
    --output-dir $OutputDir `
    --targets (Join-Path $RepoRoot "release/targets.toml")
exit $LASTEXITCODE
