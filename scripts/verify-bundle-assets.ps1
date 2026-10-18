# Every file the bundle's manifest names (its program, service definitions and screens) must be in the bundle, inside
# it. Aokie has no speech of its own, so there is no speech library, voice server or model list to look for.
param([Parameter(Mandatory = $true)][string]$Bundle)

$ErrorActionPreference = 'Stop'
$bundleRoot = (Resolve-Path -LiteralPath $Bundle).Path
$bundlePrefix = $bundleRoot.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
$manifest = Get-Content -LiteralPath (Join-Path $bundleRoot 'manifest.json') -Raw | ConvertFrom-Json
$references = @($manifest.entry.command)
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
