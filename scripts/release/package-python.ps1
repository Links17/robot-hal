param(
    [Parameter(Position = 0)][string]$Tag,
    [Parameter(Position = 1)][string]$CandidateDir,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Remaining
)

$ErrorActionPreference = "Stop"
if (
    [string]::IsNullOrEmpty($Tag) -or
    [string]::IsNullOrEmpty($CandidateDir) -or
    $Remaining.Count -ne 0
) {
    [Console]::Error.WriteLine("release.tool.invalid: expected tag new-candidate-dir")
    exit 1
}

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot = (Resolve-Path (Join-Path $ScriptDir "../..")).Path

& python $ScriptDir/release_tool.py package-python `
    --tag $Tag `
    --project (Join-Path $RepoRoot "bindings/python") `
    --candidate-dir $CandidateDir
exit $LASTEXITCODE
