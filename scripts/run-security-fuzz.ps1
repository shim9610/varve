param(
    [ValidateSet("all", "native_arbitrary", "codec_arbitrary", "layout_arbitrary", "matrix_arbitrary", "sidecar_arbitrary", "sidecar_mutation", "sidecar_state_machine")]
    [string]$Target = "all",
    [ValidateRange(1, 86400)]
    [int]$Seconds = 60,
    [ValidateRange(1, 16)]
    [int]$Jobs = 1
)

$ErrorActionPreference = "Stop"

# F-09. The documented gate is "a nonzero exit **or** a file under
# fuzz/artifacts is a failed gate", and this script used to check only the exit
# codes: a harness dropped an existing reproducer under fuzz/artifacts, ran this
# script with a successful Cargo stub, and the script exited 0 and left the
# artifact there.
#
# The gate is enforced at both ends, and it means ANY artifact, not only a newly
# produced one:
#
#   * before the campaign, an artifact that is already present fails the run
#     (exit 2) without starting Cargo. A reproducer that has not been promoted
#     to a deterministic regression is unfinished work, and letting a campaign
#     run on top of it is how a stale artifact comes to look like a fresh one;
#   * after each target, any artifact present fails the run (exit 3), naming the
#     files.
#
# Nothing here deletes anything. A fuzzer reproducer is the only copy of an
# input that reached a defect, so clearing the directory is the operator's
# explicit act, after the artifact has been promoted.
$artifactRoot = Join-Path $PSScriptRoot "..\fuzz\artifacts"

function Get-FuzzArtifacts {
    if (-not (Test-Path -LiteralPath $artifactRoot)) {
        return @()
    }
    @(Get-ChildItem -LiteralPath $artifactRoot -Recurse -File -Force |
        Sort-Object FullName |
        ForEach-Object { $_.FullName })
}

$existingArtifacts = Get-FuzzArtifacts
if ($existingArtifacts.Count -gt 0) {
    Write-Host "Refusing to start: fuzz/artifacts already holds $($existingArtifacts.Count) reproducer(s):"
    $existingArtifacts | ForEach-Object { Write-Host "  $_" }
    Write-Host "Promote each one to a deterministic regression test and remove it, then rerun."
    exit 2
}

function Assert-NoFuzzArtifacts {
    param([string]$Stage)

    $produced = Get-FuzzArtifacts
    if ($produced.Count -gt 0) {
        Write-Host "Gate failed after ${Stage}: $($produced.Count) reproducer(s) under fuzz/artifacts:"
        $produced | ForEach-Object { Write-Host "  $_" }
        Write-Host "Preserve and promote each reproducer to a deterministic regression before fixing it."
        exit 3
    }
}

$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path -LiteralPath $vswhere)) {
    throw "Visual Studio Installer (vswhere.exe) is required for MSVC AddressSanitizer."
}

$installation = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.ASAN -property installationPath
if (-not $installation) {
    throw "Install the Visual Studio C++ AddressSanitizer component before fuzzing."
}

$asanRuntime = Get-ChildItem -Path (Join-Path $installation "VC\Tools\MSVC") -Recurse -Filter "clang_rt.asan_dynamic-x86_64.dll" |
    Where-Object { $_.FullName -match "Hostx64\\x64" } |
    Sort-Object FullName -Descending |
    Select-Object -First 1
if (-not $asanRuntime) {
    throw "The x64 MSVC AddressSanitizer runtime was not found."
}

$env:PATH = $asanRuntime.DirectoryName + ";" + $env:PATH
$env:ASAN_OPTIONS = "halt_on_error=1:abort_on_error=1:strict_string_checks=1"

cargo +nightly run --manifest-path fuzz\Cargo.toml --example generate_corpus
if ($LASTEXITCODE -ne 0) {
    exit $LASTEXITCODE
}

$targets = if ($Target -eq "all") {
    @("native_arbitrary", "codec_arbitrary", "layout_arbitrary", "matrix_arbitrary", "sidecar_arbitrary", "sidecar_mutation", "sidecar_state_machine")
} else {
    @($Target)
}

foreach ($name in $targets) {
    $targetSeconds = if ($name -like "sidecar_*") { [Math]::Max($Seconds, 120) } else { $Seconds }
    $fuzzerArgs = @(
        "-max_total_time=$targetSeconds",
        "-timeout=10",
        "-max_len=1048576",
        "-rss_limit_mb=1024",
        "-dict=fuzz\dictionaries\varve.dict"
    )
    cargo +nightly fuzz run $name --fuzz-dir fuzz --target-dir target\fuzz-asan --jobs $Jobs -- `
        @fuzzerArgs
    $campaignExit = $LASTEXITCODE
    # Checked before the exit code is acted on, so a crashing campaign still
    # reports which reproducer it left behind rather than only its exit status.
    Assert-NoFuzzArtifacts -Stage "target $name"
    if ($campaignExit -ne 0) {
        exit $campaignExit
    }
}

Assert-NoFuzzArtifacts -Stage "all targets"
