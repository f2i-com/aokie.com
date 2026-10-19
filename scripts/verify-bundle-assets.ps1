# Every file the bundle's manifest names (its program, service definitions and screens) must be in the bundle, inside
# it, and so must the files a Windows bundle needs beside them that the manifest does not name: the elevated driver
# helper, Realtek's firmware with its config and licence, and (a production bundle, -DriverPackage) the
# Microsoft-signed driver package. Aokie has no speech of its own, so there is no speech library, voice server or
# model list to look for.
param(
    [Parameter(Mandatory = $true)][string]$Bundle,
    # A production bundle: driver-package/ carries the static INF and its Microsoft-signed catalog. A managed-beta
    # bundle has none (its helper renders and signs the selected dongle's INF on the PC).
    [switch]$DriverPackage
)

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

# Beside the manifest's files: the helper the WinUSB install elevates (found next to the plugin's program, and pinned
# into it by hash), Realtek's RTL8761B and RTL8822B firmware the radio loads at each start (firmware/rtl_bt, where realtek.rs looks
# beside the program), and in a production bundle the driver package the helper installs.
$required = @(
    'aokie-driver-helper.exe',
    'firmware/rtl_bt/rtl8761bu_fw.bin',
    'firmware/rtl_bt/rtl8761bu_config.bin',
    'firmware/rtl_bt/rtl8822b_fw.bin',
    'firmware/rtl_bt/rtl8822b_config.bin',
    'firmware/rtl_bt/LICENCE.rtlwifi_firmware.txt'
)
if ($DriverPackage) {
    $required += @('driver-package/aokie_winusb_bluetooth.inf', 'driver-package/aokie_winusb_bluetooth.cat')
} elseif (Test-Path -LiteralPath (Join-Path $bundleRoot 'driver-package')) {
    throw "driver-package/ is in a bundle verified as having none (a managed-beta bundle must not carry the production package)"
}
foreach ($file in $required) {
    $path = Join-Path $bundleRoot $file
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Required bundle file is missing: $file" }
    if ((Get-Item -LiteralPath $path).Length -eq 0) { throw "Required bundle file is empty: $file" }
}
Write-Host "Verified $($references.Count) manifest asset references and $($required.Count) bundle files in $Bundle"
