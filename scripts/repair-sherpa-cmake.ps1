# Repair the optional OS-caption probe after a voice build fails without wmic.
[CmdletBinding()]
param([string]$TargetDirectory)
$ErrorActionPreference = 'Stop'
# Resolved here, not as the parameter default: Windows PowerShell 5.1 leaves
# $PSScriptRoot empty inside param() when the script is run with -File.
if (-not $TargetDirectory) { $TargetDirectory = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) '../target' }
$targetRoot = (Resolve-Path -LiteralPath $TargetDirectory).Path
$files = Get-ChildItem -LiteralPath $targetRoot -Filter show-info.cmake -Recurse -File |
    Where-Object { $_.FullName -like '*sherpa-rs-sys-*\out\sherpa-onnx\cmake\show-info.cmake' }
$replacement = @'
if(SHERPA_ONNX_OS_TWO_LINES)
  string(REPLACE "\n" ";" SHERPA_ONNX_OS_LIST "${SHERPA_ONNX_OS_TWO_LINES}")
  list(LENGTH SHERPA_ONNX_OS_LIST SHERPA_ONNX_OS_LINE_COUNT)
  if(SHERPA_ONNX_OS_LINE_COUNT GREATER 1)
    list(GET SHERPA_ONNX_OS_LIST 1 SHERPA_ONNX_OS)
  else()
    set(SHERPA_ONNX_OS "Windows")
  endif()
else()
  set(SHERPA_ONNX_OS "Windows (wmic unavailable)")
endif()
'@
foreach ($file in $files) {
    $content = [IO.File]::ReadAllText($file.FullName)
    $patched = $false
    if (-not $content.Contains('Windows (wmic unavailable)')) {
        $pattern = 'string\(REPLACE "\\n" ";" SHERPA_ONNX_OS_LIST \$\{SHERPA_ONNX_OS_TWO_LINES\}\)\s*list\(GET SHERPA_ONNX_OS_LIST 1 SHERPA_ONNX_OS\)'
        if (-not [regex]::IsMatch($content, $pattern)) { throw "Unexpected generated CMake layout: $($file.FullName)" }
        $updated = [regex]::Replace($content, $pattern, [System.Text.RegularExpressions.MatchEvaluator]{ param($match) $replacement })
        [IO.File]::WriteAllText($file.FullName, $updated, [Text.UTF8Encoding]::new($false))
        Write-Host "Repaired optional Windows information probe: $($file.FullName)"
        $patched = $true
    }
    # Cargo retries may jump straight to --build after a failed configure.
    # Regenerate the existing cache so that retry has an install target. An
    # already-patched tree is reconfigured too when its build has no project
    # files, so a run that stopped before this step can still recover.
    $sourceDirectory = Split-Path $file.DirectoryName -Parent
    $buildDirectory = Join-Path (Split-Path $sourceDirectory -Parent) 'build'
    if (-not (Test-Path -LiteralPath (Join-Path $buildDirectory 'CMakeCache.txt'))) { continue }
    $generated = @('INSTALL.vcxproj', 'build.ninja', 'Makefile') | Where-Object { Test-Path -LiteralPath (Join-Path $buildDirectory $_) }
    if (-not $patched -and $generated) { continue }
    # CMake prints developer warnings on stderr. Under Windows PowerShell 5.1
    # with 'Stop', a native command's stderr is a terminating error, which
    # aborted this step before it ran; judge CMake by its exit code instead.
    $ErrorActionPreference = 'Continue'
    & cmake -S $sourceDirectory -B $buildDirectory 2>&1 | ForEach-Object { "$_" } | Out-Null
    $cmakeExit = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($cmakeExit -ne 0) { throw "CMake reconfiguration failed: $buildDirectory" }
    Write-Host "Reconfigured: $buildDirectory"
}
if (-not $files) { throw 'No generated sherpa CMake file found. Run the voice build first.' }
