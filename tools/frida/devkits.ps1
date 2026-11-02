# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Downloads the pinned, checksum-verified Frida devkits for Windows x64: the
# PowerShell twin of devkits.sh, for a box without Git Bash. Same version,
# same checksum file, same destination.
param([string]$Target = 'x86_64-pc-windows-msvc')
$ErrorActionPreference = 'Stop'
$root = Resolve-Path (Join-Path $PSScriptRoot '..\..')
$version = '17.17.0'
switch ($Target) {
    'x86_64-pc-windows-msvc' { $platform = 'windows-x86_64' }
    default { throw "Unsupported native Frida target for this script: $Target (use devkits.sh elsewhere)" }
}
# Extracts a .tar.xz. Windows' own tar.exe (System32, bsdtar) is built
# without liblzma: on an .xz it spawns an external xz, which is not there,
# and waits for it forever (every CI run hung here). Git for Windows' tar
# and 7-Zip both read xz; use whichever is installed.
function Expand-TarXz([string]$Archive, [string]$Destination) {
    $gitTar = Join-Path $env:ProgramFiles 'Git\usr\bin\tar.exe'
    if (Test-Path $gitTar) {
        # GNU tar reads the drive letter of a Windows path as a remote host
        # unless told otherwise, and unescapes backslashes: give it slashes.
        & $gitTar --force-local -xf ($Archive -replace '\\', '/') -C ($Destination -replace '\\', '/')
        if ($LASTEXITCODE -ne 0) { throw "could not extract $Archive (tar exit $LASTEXITCODE)" }
        return
    }
    $sevenZip = Get-Command 7z.exe -ErrorAction SilentlyContinue
    if ($sevenZip) {
        # Two passes: .tar.xz -> .tar, then .tar -> files.
        & $sevenZip.Source x $Archive "-o$Destination" -y | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "could not decompress $Archive (7z exit $LASTEXITCODE)" }
        $tar = Join-Path $Destination ([IO.Path]::GetFileNameWithoutExtension($Archive))
        & $sevenZip.Source x $tar "-o$Destination" -y | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "could not extract $tar (7z exit $LASTEXITCODE)" }
        Remove-Item $tar
        return
    }
    throw "no extractor for $Archive : install Git for Windows or 7-Zip. Windows' tar.exe has no xz support and hangs on this archive."
}

$kits = if ($env:PERFR_FRIDA_DEVKIT_ROOT) { $env:PERFR_FRIDA_DEVKIT_ROOT } else { Join-Path $root 'target\frida-devkits' }
$sums = Get-Content (Join-Path $PSScriptRoot 'devkits.sha256')
foreach ($component in 'core', 'gum') {
    $name = "frida-$component-devkit-$version-$platform.tar.xz"
    $kit = Join-Path $kits "$Target\$component"
    if (Test-Path (Join-Path $kit ".verified-$version")) { continue }
    New-Item -ItemType Directory -Force (Join-Path $kits $Target) | Out-Null
    $download = Join-Path $kits "$Target\$name"
    $uri = "https://github.com/frida/frida/releases/download/$version/$name"
    # curl, which Windows 10+ ships, with a hard stall limit: Invoke-WebRequest
    # once sat 88 minutes on this download in CI, past its own -TimeoutSec,
    # which does not cover a response body that stops arriving.
    $curl = Join-Path (Join-Path $env:SystemRoot 'System32') 'curl.exe'
    Write-Host "Downloading $name"
    & $curl -fsSL --retry 3 --retry-delay 5 --max-time 900 --speed-time 60 --speed-limit 10000 -o $download $uri
    if ($LASTEXITCODE -ne 0) { Remove-Item $download -ErrorAction SilentlyContinue; throw "could not download $uri (curl exit $LASTEXITCODE)" }
    $expected = ($sums | Where-Object { $_ -match "\s$([regex]::Escape($name))$" } | ForEach-Object { ($_ -split '\s+')[0] })
    $actual = (Get-FileHash -Algorithm SHA256 $download).Hash.ToLowerInvariant()
    if (-not $expected -or $actual -ne $expected) { Remove-Item $download; throw "Invalid checksum: $name" }
    New-Item -ItemType Directory -Force $kit | Out-Null
    Expand-TarXz $download $kit
    New-Item -ItemType File -Force (Join-Path $kit ".verified-$version") | Out-Null
    Remove-Item $download
}
Write-Host "Frida $version devkits ready under $kits\$Target"
