param(
    [ValidateSet("all", "native_arbitrary", "codec_arbitrary", "layout_arbitrary", "matrix_arbitrary")]
    [string]$Target = "all",
    [ValidateRange(1, 86400)]
    [int]$Seconds = 60,
    [ValidateRange(1, 16)]
    [int]$Jobs = 1
)

$ErrorActionPreference = "Stop"

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
    @("native_arbitrary", "codec_arbitrary", "layout_arbitrary", "matrix_arbitrary")
} else {
    @($Target)
}

foreach ($name in $targets) {
    $fuzzerArgs = @(
        "-max_total_time=$Seconds",
        "-timeout=10",
        "-max_len=1048576",
        "-dict=fuzz\dictionaries\varve.dict"
    )
    cargo +nightly fuzz run $name --fuzz-dir fuzz --target-dir target\fuzz-asan --jobs $Jobs -- `
        @fuzzerArgs
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
}
