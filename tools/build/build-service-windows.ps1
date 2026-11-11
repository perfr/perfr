# Copyright (c) 2026 The perfr Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Builds the service, the embedded viewer, the Frida files and the manual
# instrumentation SDK on Windows x64, into dist\windows\. The twin of
# tools/build/build-service-macos.sh and tools/build/build-service-musl.sh.
#
#   .\tools\build\build-service-windows.ps1            # dist\windows
#   .\tools\build\build-service-windows.ps1 -Output d  # elsewhere
#
# Runs from the repository root, where .cargo\config.toml adds the
# static-CRT rustflags to every cargo invocation.
#
# Needs Rust 1.88.0 (rustup), Visual Studio 2022 with the C++ workload (for
# the Frida adapters, through the cc crate) and network access the first time
# (the Frida devkits). Every binary links the C runtime statically
# (.cargo\config.toml), so the result depends on system DLLs only; the
# script checks that with dumpbin.
param([string]$Output = 'dist\windows')
$ErrorActionPreference = 'Stop'
Push-Location (Join-Path $PSScriptRoot '..\..')
try {
    if (-not (Test-Path 'crates\perfr-live-viewer\viewer-dist\index.html')) {
        throw 'crates\perfr-live-viewer\viewer-dist\index.html is missing: the service embeds the viewer at build time'
    }
    . .\tools\windows\msvc-env.ps1
    # rustup-init puts the toolchain in %USERPROFILE%\.cargo\bin; the PATH
    # entry is missing on some machines, so look there ourselves.
    if (-not (Get-Command rustup.exe -ErrorAction SilentlyContinue)) {
        $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
        if (-not (Test-Path (Join-Path $cargoBin 'rustup.exe'))) {
            throw "rustup is not on PATH and not in $cargoBin; install Rust from https://rustup.rs"
        }
        $env:PATH = "$cargoBin;$env:PATH"
    }
    $target = 'x86_64-pc-windows-msvc'
    rustup target add --toolchain 1.88.0 $target
    & .\tools\frida\devkits.ps1 $target
    if ($LASTEXITCODE -ne 0) { throw 'Frida devkits' }

    cargo +1.88.0 build --locked --release --target $target -p perfr
    if ($LASTEXITCODE -ne 0) { throw 'perfr build failed' }
    cargo +1.88.0 build --locked --release --target $target -p perfr-api -p perfr-test-rust -p perfr-lib-windows --features perfr-lib-windows/native
    if ($LASTEXITCODE -ne 0) { throw 'perfr-api / libperfr build failed' }
    cargo +1.88.0 build --locked --release --target $target -p perfr-hook
    if ($LASTEXITCODE -ne 0) { throw 'perfr-hook build failed' }

    New-Item -ItemType Directory -Force "$Output\include", "$Output\licenses" | Out-Null
    $files = @{
        "target\$target\release\perfr.exe" = 'perfr.exe'
        "target\$target\release\perfr-hook.exe" = 'perfr-hook.exe'
        "target\$target\release\libperfr.dll" = 'libperfr.dll'
        "target\$target\release\libperfr.dll.lib" = 'libperfr.dll.lib'
        "target\$target\release\perfr_api.lib" = 'perfr_api.lib'
        "target\$target\release\PerfrTestRust.exe" = 'PerfrTestRust.exe'
        'crates\perfr-api\include\perfr.h' = 'include\perfr.h'
        'crates\perfr-api\python\perfr_api\__init__.py' = 'perfr_api.py'
        # What a user unpacks, not how a contributor builds.
        'tools\release\README.archive.md' = 'README.md'
    }
    foreach ($entry in $files.GetEnumerator()) {
        if (-not (Test-Path $entry.Key)) { throw "missing $($entry.Key)" }
        Copy-Item $entry.Key (Join-Path $Output $entry.Value) -Force
    }
    # Release builds compile full debug info. The PDBs stay out of the core
    # zip; the release workflow publishes them. Copy every PDB next to the
    # binary so that step can find the one the PE names.
    Get-ChildItem "target\$target\release\*.pdb" | ForEach-Object {
        Copy-Item $_.FullName (Join-Path $Output $_.Name) -Force
    }
    if (-not (Test-Path (Join-Path $Output 'perfr.pdb'))) {
        throw 'perfr.pdb is missing; a release build keeps full debug info in the PDB'
    }
    Copy-Item 'tools\frida\licenses\*' "$Output\licenses\" -Force

    # Static CRT: nothing but system DLLs may be imported.
    foreach ($binary in 'perfr.exe', 'perfr-hook.exe', 'libperfr.dll') {
        $imports = (dumpbin /nologo /dependents (Join-Path $Output $binary) | Select-String -Pattern '^\s+(\S+\.dll)\s*$' | ForEach-Object { $_.Matches[0].Groups[1].Value.ToLowerInvariant() })
        $runtime = $imports | Where-Object { $_ -like 'vcruntime*' -or $_ -like 'msvcp*' -or $_ -like 'msvcr*' -or $_ -like 'api-ms-win-crt-*' -or $_ -eq 'ucrtbase.dll' }
        if ($runtime) { throw "$binary links the C runtime dynamically: $($runtime -join ', ')" }
        Write-Host ("  {0}: {1}" -f $binary, ($imports -join ' '))
    }
    Write-Host "Windows service and SDK in $Output"
} finally {
    Pop-Location
}
