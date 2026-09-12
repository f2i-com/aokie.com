param([Parameter(Mandatory = $true)][string]$Bundle, [switch]$Voice)

$ErrorActionPreference = 'Stop'
$bundleRoot = (Resolve-Path -LiteralPath $Bundle).Path
$bundlePrefix = $bundleRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
$manifest = Get-Content -LiteralPath (Join-Path $bundleRoot 'manifest.json') -Raw | ConvertFrom-Json
$references = @($manifest.entry.command)
if ($Voice) {
    # sherpa's unversioned ORT cannot run the newer Pocket/Parakeet engines.
    $references += @('aokie-voice-server.exe', 'models-manifest.json',
        'onnxruntime_1.25.0.dll', 'onnxruntime.dll',
        'sherpa-onnx-c-api.dll', 'sherpa-onnx-cxx-api.dll')
}
$references += @($manifest.serviceDefinitions | ForEach-Object { $_.definitionFile })
foreach ($screen in $manifest.ui.screens) {
    $references += $screen.entry
    $references += @($screen.files)
}
foreach ($reference in ($references | Select-Object -Unique)) {
    if ([string]::IsNullOrWhiteSpace($reference) -or [IO.Path]::IsPathRooted($reference) -or $reference.Contains(':')) {
        throw "Invalid manifest asset path: $reference"
    }
    $assetPath = [IO.Path]::GetFullPath((Join-Path $bundleRoot $reference))
    if (-not $assetPath.StartsWith($bundlePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "Manifest asset escapes bundle: $reference"
    }
    if (-not (Test-Path -LiteralPath $assetPath -PathType Leaf)) {
        throw "Required manifest asset is missing: $reference"
    }
}
foreach ($definition in $manifest.serviceDefinitions) {
    $document = Get-Content -LiteralPath (Join-Path $bundleRoot $definition.definitionFile) -Raw | ConvertFrom-Json
    if (-not $document.id -or -not $document.name) { throw "Invalid service definition: $($definition.definitionFile)" }
}
Write-Host "Verified $($references.Count) manifest asset references in $Bundle"
