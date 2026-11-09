param([switch]$SkipTests)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
Push-Location $root
try {
    # Windows cannot replace a running executable, so a running engine would leave
    # the old dist\github-turbo.exe in place. Say so before spending time building.
    $distExe = [IO.Path]::GetFullPath((Join-Path $root 'dist\github-turbo.exe'))
    $running = @(Get-CimInstance Win32_Process -Filter "name='github-turbo.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -and ([IO.Path]::GetFullPath($_.ExecutablePath) -eq $distExe) })
    if ($running) {
        throw "dist\github-turbo.exe is running (PID $(($running.ProcessId) -join ', ')). Run dist\Stop.cmd first, then build again and start with dist\Start.cmd."
    }
    if (!$SkipTests) {
        cargo test --locked --target x86_64-pc-windows-msvc
        if ($LASTEXITCODE -ne 0) { throw 'Tests failed' }
    }
    cargo build --release --locked --target x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
    New-Item -ItemType Directory -Force -Path (Join-Path $root 'dist') | Out-Null
    Copy-Item -LiteralPath (Join-Path $root 'target\x86_64-pc-windows-msvc\release\github-turbo.exe') -Destination (Join-Path $root 'dist\github-turbo.exe')
    Copy-Item -LiteralPath (Join-Path $root 'scripts\windows.ps1') -Destination (Join-Path $root 'dist\windows.ps1')
    foreach ($entry in @(@('Start','enable'),@('Stop','disable'),@('Recover','recover'))) {
        [IO.File]::WriteAllText((Join-Path $root ('dist\' + $entry[0] + '.cmd')),('@echo off' + "`r`n" + 'powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0windows.ps1" -Action ' + $entry[1] + "`r`n" + 'if errorlevel 1 pause' + "`r`n"),[Text.Encoding]::ASCII)
    }
    if (!(Test-Path -LiteralPath (Join-Path $root 'dist\config.json'))) {
        Copy-Item -LiteralPath (Join-Path $root 'config.json') -Destination (Join-Path $root 'dist\config.json')
    }
    Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination (Join-Path $root 'dist\README.md')
    Copy-Item -LiteralPath (Join-Path $root 'README_zh.md') -Destination (Join-Path $root 'dist\README_zh.md')
    Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $root 'dist\LICENSE')
    Copy-Item -LiteralPath (Join-Path $root 'third-party') -Destination (Join-Path $root 'dist') -Recurse -Force
    New-Item -ItemType Directory -Force -Path (Join-Path $root 'dist\docs') | Out-Null
    Copy-Item -Path (Join-Path $root 'docs\*.md') -Destination (Join-Path $root 'dist\docs')
    New-Item -ItemType Directory -Force -Path (Join-Path $root 'dist\scripts') | Out-Null
    Copy-Item -LiteralPath (Join-Path $root 'scripts\download.ps1') -Destination (Join-Path $root 'dist\scripts\download.ps1')
    Copy-Item -LiteralPath (Join-Path $root 'scripts\hosts.ps1') -Destination (Join-Path $root 'dist\scripts\hosts.ps1')
    # Only crates that can be built into the Windows executable, not every platform's.
    $metadata = cargo metadata --format-version 1 --locked --filter-platform x86_64-pc-windows-msvc | ConvertFrom-Json
    if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
    $licensesDir = Join-Path $root 'dist\licenses'
    # Regenerate from scratch so crates dropped from Cargo.lock leave no notices behind.
    if (Test-Path -LiteralPath $licensesDir) { Remove-Item -LiteralPath $licensesDir -Recurse -Force }
    $unlicensed = @()
    $inventory = foreach ($package in $metadata.packages) {
        if ($package.name -eq 'github-turbo') { continue }
        $id = $package.name + '-' + $package.version
        $noticeDir = Join-Path $licensesDir $id
        New-Item -ItemType Directory -Force -Path $noticeDir | Out-Null
        Get-ChildItem -LiteralPath (Split-Path $package.manifest_path -Parent) -File | Where-Object Name -match '^(LICENSE|COPYING|NOTICE|COPYRIGHT)' | Copy-Item -Destination $noticeDir
        # Some published crates omit their license files; assets\crate-licenses holds
        # the upstream texts for those, taken from the crate's own repository.
        $supplied = Join-Path $root ('assets\crate-licenses\' + $id)
        if (!(Get-ChildItem -LiteralPath $noticeDir -File) -and (Test-Path -LiteralPath $supplied)) {
            Get-ChildItem -LiteralPath $supplied -File | Copy-Item -Destination $noticeDir
        }
        if (!(Get-ChildItem -LiteralPath $noticeDir -File)) { $unlicensed += $id }
        [pscustomobject]@{name=$package.name;version=$package.version;license=$package.license;repository=$package.repository}
    }
    if ($unlicensed) {
        throw "No license text for: $($unlicensed -join ', '). Add the upstream files under assets\crate-licenses\<name>-<version>."
    }
    $inventory | ConvertTo-Json -Depth 3 | Set-Content -Encoding utf8 (Join-Path $root 'dist\dependencies.json')
    Get-FileHash -LiteralPath (Join-Path $root 'dist\github-turbo.exe') -Algorithm SHA256
} finally { Pop-Location }
