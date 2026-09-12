# Repair the optional OS-caption probe after a voice build fails without wmic.
[CmdletBinding()]
param([string]$TargetDirectory = (Join-Path $PSScriptRoot '../target'))
$ErrorActionPreference = 'Stop'
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
    if ($content.Contains('Windows (wmic unavailable)')) { continue }
    $pattern = 'string\(REPLACE "\\n" ";" SHERPA_ONNX_OS_LIST \$\{SHERPA_ONNX_OS_TWO_LINES\}\)\s*list\(GET SHERPA_ONNX_OS_LIST 1 SHERPA_ONNX_OS\)'
    if (-not [regex]::IsMatch($content, $pattern)) { throw "Unexpected generated CMake layout: $($file.FullName)" }
    $updated = [regex]::Replace($content, $pattern, [System.Text.RegularExpressions.MatchEvaluator]{ param($match) $replacement })
    [IO.File]::WriteAllText($file.FullName, $updated, [Text.UTF8Encoding]::new($false))
    Write-Host "Repaired optional Windows information probe: $($file.FullName)"
    # Cargo retries may jump straight to --build after a failed configure.
    # Regenerate the existing cache so that retry has an install target.
    $sourceDirectory = Split-Path $file.DirectoryName -Parent
    $buildDirectory = Join-Path (Split-Path $sourceDirectory -Parent) 'build'
    if (Test-Path -LiteralPath (Join-Path $buildDirectory 'CMakeCache.txt')) {
        & cmake -S $sourceDirectory -B $buildDirectory
        if ($LASTEXITCODE -ne 0) { throw "CMake reconfiguration failed: $buildDirectory" }
    }
}
if (-not $files) { throw 'No generated sherpa CMake file found. Run the voice build first.' }
