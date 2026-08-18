param(
    [Parameter(Position = 0)][string]$Tag,
    [Parameter(Position = 1)][string]$OutputDir,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Remaining
)

$ErrorActionPreference = "Stop"
if (
    [string]::IsNullOrEmpty($Tag) -or
    [string]::IsNullOrEmpty($OutputDir) -or
    $Remaining.Count -ne 0
) {
    [Console]::Error.WriteLine("release.tool.invalid: expected tag output-dir")
    exit 1
}

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir "../..")).Path

& python $ScriptDir/release_tool.py package-python `
    --tag $Tag `
    --project (Join-Path $RepoRoot "bindings/python") `
    --output-dir $OutputDir
exit $LASTEXITCODE
