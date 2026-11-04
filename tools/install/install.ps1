# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Installs the Rust perfr on Windows x64 from the latest release, which
# perfr.dev serves under /download/: the twin of install.sh. Downloads perfr-windows-x86_64.zip and its
# .sha256, refuses to install without a matching checksum, unpacks into
# %LOCALAPPDATA%\Programs\perfr (perfr.exe, perfr_api.dll and perfr.h beside it),
# and says how to put it on PATH. Function hooks are a separate zip. Set
# PERFR_WITH_HOOKS=1, or pass -WithHooks / --with-hooks, to install them too.
# `perfr hooks install` does the same later.
#
#   irm https://perfr.dev/install.ps1 | iex
#   $env:PERFR_WITH_HOOKS = '1'; irm https://perfr.dev/install.ps1 | iex
#
# Environment overrides, as for install.sh: PERFR_INSTALL_BASE (the URL the
# assets are under), PERFR_VERSION (a tag such as perfr-v0.1.0; default
# latest), PERFR_INSTALL_DIR (default %LOCALAPPDATA%\Programs\perfr),
# PERFR_WITH_HOOKS=1.
$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

function Say([string]$message) { Write-Host "perfr: $message" }
function Die([string]$message) { Write-Error "perfr: error: $message"; exit 1 }

if ([IntPtr]::Size -ne 8 -or $env:PROCESSOR_ARCHITECTURE -notin 'AMD64', 'ARM64') {
    Die 'only 64-bit Windows is supported'
}
if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    Say 'this is an ARM64 machine; the x64 build runs under emulation and the kernel logger is untested there'
}

$withHooks = $env:PERFR_WITH_HOOKS -in @('1', 'true', 'yes', 'TRUE', 'YES') `
    -or ($args -contains '--with-hooks') -or ($args -contains '-WithHooks')

$base = if ($env:PERFR_INSTALL_BASE) { $env:PERFR_INSTALL_BASE }
        elseif ($env:PERFR_VERSION) { "https://github.com/perfr/perfr/releases/download/$($env:PERFR_VERSION)" }
        else { 'https://perfr.dev/download' }
$version = if ($env:PERFR_VERSION) { $env:PERFR_VERSION } else { 'latest' }
$installDir = if ($env:PERFR_INSTALL_DIR) { $env:PERFR_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\perfr' }
$asset = 'perfr-windows-x86_64.zip'
$url = "$base/$asset"

Say "installing perfr $version for Windows x64"
$temp = Join-Path ([IO.Path]::GetTempPath()) ("perfr-install-" + [guid]::NewGuid().ToString('n'))
New-Item -ItemType Directory -Force $temp | Out-Null
try {
    $archive = Join-Path $temp $asset
    try {
        Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing
    } catch {
        Die ("could not download $url`n" +
             "The Rust release is tagged perfr-vX.Y.Z (not v1.* or nightly-*, which are the legacy C++ build).`n" +
             "Until such a tag exists there is nothing to install; build from source with .\tools\build\build-service-windows.ps1.")
    }
    function Verify-Sha256([string]$file, [string]$fileUrl, [string]$label) {
        $sumUrl = "$($fileUrl).sha256"
        $sumFile = "$($file).sha256"
        try {
            Invoke-WebRequest -Uri $sumUrl -OutFile $sumFile -UseBasicParsing
        } catch {
            Die "no checksum file at $sumUrl; refusing to install an unverified archive"
        }
        $expected = ((Get-Content $sumFile -Raw) -split '\s+')[0].ToLowerInvariant()
        if ($expected -notmatch '^[0-9a-f]{64}$') { Die "malformed checksum file for $label" }
        $actual = (Get-FileHash -Algorithm SHA256 $file).Hash.ToLowerInvariant()
        if ($actual -ne $expected) { Die "checksum mismatch for $label (expected $expected, got $actual)" }
        Say 'checksum verified'
    }
    Verify-Sha256 $archive $url $asset

    $unpacked = Join-Path $temp 'unpacked'
    Expand-Archive -Path $archive -DestinationPath $unpacked -Force
    $root = Join-Path $unpacked 'perfr'
    foreach ($required in 'perfr.exe', 'perfr_api.dll') {
        if (-not (Test-Path (Join-Path $root $required))) { Die "archive is missing $required" }
    }
    New-Item -ItemType Directory -Force $installDir | Out-Null
    # A running service holds its files; stop it before replacing them.
    Get-Process -Name 'perfr' -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$installDir*" } | ForEach-Object {
        Say "stopping perfr (pid $($_.Id)) in $installDir"
        $_ | Stop-Process -Force
    }
    # Copy the core tree. An older archive may still contain the Frida files;
    # those get copied too. A current archive leaves them for --with-hooks.
    Copy-Item -Path (Join-Path $root '*') -Destination $installDir -Recurse -Force
    Say "installed to $installDir"
    if ($withHooks) {
        $hooksAsset = 'perfr-hooks-windows-x86_64.zip'
        $hooksUrl = "$base/$hooksAsset"
        Say "installing function hooks from $hooksUrl"
        $hooksArchive = Join-Path $temp $hooksAsset
        try {
            Invoke-WebRequest -Uri $hooksUrl -OutFile $hooksArchive -UseBasicParsing
        } catch {
            Die ("could not download $hooksUrl`n" +
                 "The core install is already in place. Function hooks are perfr-hooks-windows-x86_64.zip on the same release. Run ``perfr hooks install``.")
        }
        Verify-Sha256 $hooksArchive $hooksUrl $hooksAsset
        $hooksUnpacked = Join-Path $temp 'hooks'
        Expand-Archive -Path $hooksArchive -DestinationPath $hooksUnpacked -Force
        $hooksRoot = Join-Path $hooksUnpacked 'perfr-hooks'
        foreach ($required in 'perfr-frida-helper.exe', 'perfr_frida_agent.dll') {
            if (-not (Test-Path (Join-Path $hooksRoot $required))) { Die "hooks archive is missing $required" }
        }
        Copy-Item -Path (Join-Path $hooksRoot '*') -Destination $installDir -Recurse -Force
        Say "installed function hooks to $installDir"
    }
} finally {
    Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
}

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (($userPath -split ';') -notcontains $installDir) {
    Say "note: $installDir is not on your PATH. Add it for this user with:"
    Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"$installDir;`" + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
}

Write-Host @"

perfr is installed. Start it and open the viewer in a browser:

  perfr
  # then open http://127.0.0.1:44766/

Manual scopes need no privilege. Function hooks are optional; this install
included them only when PERFR_WITH_HOOKS=1 (or -WithHooks). Otherwise run:

  perfr hooks install

perfr looks for perfr_frida_agent.dll and perfr-frida-helper.exe beside
itself, or in %LOCALAPPDATA%\perfr\hooks. Call-stack
sampling, the scheduler and thread states come from the Windows kernel
logger (ETW), which needs an elevated prompt: start perfr from a
terminal opened with "Run as administrator" to record them. Auto-profiling
and the kernel hook engine are Linux only.

To instrument your own code: include\perfr.h is beside perfr
(nothing to link; it finds perfr_api.dll there), or use the perfr-api crate
or the perfr_api.py module.
"@
