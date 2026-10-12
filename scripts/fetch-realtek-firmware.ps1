# fetch-realtek-firmware.ps1 -Destination DIR: Realtek's RTL8761B (USB) Bluetooth firmware, its config, and the
# licence they are redistributed under, from linux-firmware (its official GitLab mirror, at a pinned commit), each
# checked against its pinned SHA-256. Aokie loads them into an RTL8761B dongle (0bda:8771) at each start
# (crates/aokie-bluetooth/src/aokie_radio/realtek.rs): on its ROM alone such a dongle takes Aokie's settings and is
# seen by no phone. The bundle carries them in firmware/rtl_bt/, beside the plugin's program, where the loader looks.
# The licence (Realtek's: redistribution of the unmodified binary, its notice kept) goes with them.
param([Parameter(Mandatory = $true)][string]$Destination)
$ErrorActionPreference = 'Stop'
# linux-firmware's main on 2026-10-09; the RTL8761BU firmware last changed in 1de22a39 (2023-05-03, version 0xDFC6_D922)
$commit = 'afabaf773c4c2e2c841429933a6a084a5af4d14d'
$files = [ordered]@{
    'rtl_bt/rtl8761bu_fw.bin'               = '1d7a9597349ad89344fa16c1913d3e39e9a12e966e417ca16871bc79bbe59edb'
    'rtl_bt/rtl8761bu_config.bin'           = '6c28a3f07c6a30ed208c4b64862a23f02b7d93543ea980edd24df16bab45095f'
    'LICENSES/LICENCE.rtlwifi_firmware.txt' = 'a61351665b4f264f6c631364f85b907d8f8f41f8b369533ef4021765f9f3b62e'
}
New-Item -ItemType Directory -Force $Destination | Out-Null
foreach ($path in $files.Keys) {
    $out = Join-Path $Destination (Split-Path $path -Leaf)
    Invoke-WebRequest -UseBasicParsing -Uri "https://gitlab.com/kernel-firmware/linux-firmware/-/raw/$commit/$path" -OutFile $out
    $sha = (Get-FileHash $out -Algorithm SHA256).Hash.ToLower()
    if ($sha -ne $files[$path]) {
        Remove-Item -LiteralPath $out -Force
        throw "$path is not the pinned file (sha256 $sha)"
    }
    "{0} -> {1}" -f $path, $out
}
