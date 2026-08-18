param(
    [Parameter(Position = 0)][string]$Tag,
    [Parameter(Position = 1)][string]$Commit,
    [Parameter(Position = 2)][string]$ArtifactsDir,
    [Parameter(Position = 3)][string]$SoftwareUri,
    [Parameter(Position = 4)][string]$HardwareUri,
    [Parameter(ValueFromRemainingArguments = $true)][string[]]$Remaining
)

$ErrorActionPreference = "Stop"
if (
    [string]::IsNullOrEmpty($Tag) -or
    [string]::IsNullOrEmpty($Commit) -or
    [string]::IsNullOrEmpty($ArtifactsDir) -or
    [string]::IsNullOrEmpty($SoftwareUri) -or
    [string]::IsNullOrEmpty($HardwareUri) -or
    $Remaining.Count -ne 0
) {
    [Console]::Error.WriteLine("release.tool.invalid: expected tag commit artifacts-dir software-uri hardware-uri")
    exit 1
}

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
& python $ScriptDir/release_tool.py generate-manifest `
    --tag $Tag `
    --commit $Commit `
    --artifacts-dir $ArtifactsDir `
    --output-dir $ArtifactsDir `
    --software-qualification $SoftwareUri `
    --hardware-qualification $HardwareUri
exit $LASTEXITCODE
