<#
.SYNOPSIS
Build a numbered release candidate (RC) package without touching final releases.

.DESCRIPTION
Runs every quality gate (clean tree, rustfmt, clippy, tests via build.ps1), stages
the same files as package.ps1, and writes
packages\rc\GitHubTurbo-<version>-rc.<n>-windows-x64.zip with a .sha256 file and a
.json manifest (commit, tool versions, per-file SHA256). It then extracts the zip
to a scratch folder and runs the packaged executable to check it starts.

The version comes from Cargo.toml. If a final package for that version already
exists in packages\, the script stops: bump Cargo.toml first, so an RC never
carries the number of a release that already shipped.

.PARAMETER Rc
RC number. Default: one more than the highest existing RC for this version.

.PARAMETER AllowDirty
Package even with uncommitted changes. The manifest records "dirty": true.

.PARAMETER SkipChecks
Skip rustfmt, clippy and tests (build only). Recorded in the manifest.

.PARAMETER NetworkSmoke
Also run "github-turbo git ls-remote" against a public GitHub repository.

.EXAMPLE
.\scripts\package-rc.ps1
.\scripts\package-rc.ps1 -AllowDirty -NetworkSmoke
#>
param(
    [int]$Rc = 0,
    [switch]$AllowDirty,
    [switch]$SkipChecks,
    [switch]$NetworkSmoke
)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$target = 'x86_64-pc-windows-msvc'

function Invoke-Checked([string]$what, [scriptblock]$command) {
    Write-Host "==> $what"
    & $command
    if ($LASTEXITCODE -ne 0) { throw "$what failed (exit $LASTEXITCODE)" }
}

Push-Location $root
try {
    # --- Version and RC number -------------------------------------------------
    $version = ((Get-Content -Raw 'Cargo.toml') | Select-String '(?m)^version = "([^"]+)"').Matches[0].Groups[1].Value
    if ($version -match '-') { throw "Cargo.toml version '$version' already has a pre-release suffix; use a plain x.y.z" }
    if (Test-Path -LiteralPath "packages\GitHubTurbo-$version-windows-x64.zip") {
        throw "A final package for $version already exists in packages\. Bump the version in Cargo.toml (and README headings) before cutting an RC."
    }
    $rcDir = Join-Path $root 'packages\rc'
    New-Item -ItemType Directory -Force -Path $rcDir | Out-Null
    if ($Rc -le 0) {
        $existing = Get-ChildItem -LiteralPath $rcDir -Filter "GitHubTurbo-$version-rc.*-windows-x64.zip" -File |
            ForEach-Object { if ($_.Name -match '-rc\.(\d+)-windows') { [int]$Matches[1] } }
        $Rc = 1 + (@($existing) + 0 | Measure-Object -Maximum).Maximum
    }
    $label = "$version-rc.$Rc"
    $name = "GitHubTurbo-$label-windows-x64"
    $archive = Join-Path $rcDir "$name.zip"
    if (Test-Path -LiteralPath $archive) { throw "$name.zip already exists; RC packages are never overwritten. Pick another -Rc." }
    Write-Host "Release candidate $label"

    # --- Source state ----------------------------------------------------------
    $commit = (git rev-parse HEAD).Trim()
    $status = @(git status --porcelain)
    $dirty = $status.Count -gt 0
    if ($dirty -and !$AllowDirty) {
        $status | Select-Object -First 20 | ForEach-Object { Write-Host "  $_" }
        throw 'Uncommitted changes. Commit them, or pass -AllowDirty to package anyway (recorded in the manifest).'
    }
    $heading = (Get-Content -LiteralPath 'README.md' -TotalCount 1)
    if ($heading -notmatch [regex]::Escape($version)) {
        Write-Warning "README.md heading '$heading' does not mention $version"
    }

    # --- Quality gates and build -----------------------------------------------
    if (!$SkipChecks) {
        Invoke-Checked 'rustfmt' { cargo fmt --check }
        Invoke-Checked 'clippy' { cargo clippy --all-targets --locked --target $target -- -D warnings }
        & (Join-Path $PSScriptRoot 'build.ps1')            # runs the tests, then the release build
    } else {
        & (Join-Path $PSScriptRoot 'build.ps1') -SkipTests
    }

    # --- Stage (same content as package.ps1) -----------------------------------
    $stage = Join-Path $root "packages\staging\$name"
    if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    foreach ($file in @('github-turbo.exe', 'windows.ps1', 'Start.cmd', 'Stop.cmd', 'Recover.cmd', 'README.md', 'LICENSE', 'dependencies.json')) {
        Copy-Item -LiteralPath (Join-Path $root "dist\$file") -Destination $stage
    }
    Copy-Item -LiteralPath 'README_zh.md' -Destination $stage
    Copy-Item -LiteralPath 'third-party' -Destination $stage -Recurse -Force
    # The checked-in defaults, never this machine's settings.
    Copy-Item -LiteralPath 'config.json' -Destination (Join-Path $stage 'config.json')
    Copy-Item -LiteralPath 'dist\licenses' -Destination $stage -Recurse -Force
    New-Item -ItemType Directory -Force -Path (Join-Path $stage 'scripts') | Out-Null
    foreach ($file in @('download.ps1', 'hosts.ps1')) {
        Copy-Item -LiteralPath "scripts\$file" -Destination (Join-Path $stage 'scripts')
    }
    Copy-Item -LiteralPath 'dist\docs' -Destination $stage -Recurse -Force
    Get-ChildItem -LiteralPath $stage -Recurse -File |
        Where-Object { $_.LastWriteTimeUtc -lt [datetime]'1981-01-01' } |
        ForEach-Object { $_.LastWriteTimeUtc = [datetime]'2000-01-01' }

    # --- Manifest --------------------------------------------------------------
    $files = Get-ChildItem -LiteralPath $stage -Recurse -File | Sort-Object FullName | ForEach-Object {
        [ordered]@{
            path   = $_.FullName.Substring($stage.Length + 1) -replace '\\', '/'
            bytes  = $_.Length
            sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }
    $manifest = [ordered]@{
        name       = $name
        version    = $version
        rc         = $Rc
        commit     = $commit
        dirty      = $dirty
        checks     = $(if ($SkipChecks) { 'skipped' } else { 'rustfmt, clippy -D warnings, cargo test' })
        built_utc  = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
        rustc      = (rustc --version).Trim()
        target     = $target
        files      = @($files)
    }
    # A copy inside the package lets testers report exactly what they ran.
    $manifestJson = $manifest | ConvertTo-Json -Depth 4
    [IO.File]::WriteAllText((Join-Path $stage 'RELEASE-CANDIDATE.json'), $manifestJson, (New-Object Text.UTF8Encoding $false))

    # --- Archive and privacy check ---------------------------------------------
    Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $archive
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($archive)
    try {
        $names = @($zip.Entries | ForEach-Object { $_.FullName -replace '\\', '/' })
    } finally { $zip.Dispose() }
    $leak = $names | Where-Object { $_ -match '(^|/)(state|reports|work|\.git)/|\.(pem|key|body|headers|log)$|session\.json' }
    if ($leak) {
        Remove-Item -LiteralPath $archive -Force
        throw "Private machine state would be packaged: $($leak -join ', ')"
    }
    foreach ($required in @('github-turbo.exe', 'Start.cmd', 'Stop.cmd', 'Recover.cmd', 'windows.ps1', 'config.json', 'RELEASE-CANDIDATE.json')) {
        if ($names -notcontains $required) {
            Remove-Item -LiteralPath $archive -Force
            throw "Incomplete archive: $required is missing"
        }
    }

    # --- Smoke test the packaged copy ------------------------------------------
    $verify = Join-Path $root "packages\verify\$name"
    if (Test-Path -LiteralPath $verify) { Remove-Item -LiteralPath $verify -Recurse -Force }
    Expand-Archive -LiteralPath $archive -DestinationPath $verify
    $exe = Join-Path $verify 'github-turbo.exe'
    $builtHash = (Get-FileHash -LiteralPath 'dist\github-turbo.exe' -Algorithm SHA256).Hash
    if ((Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash -ne $builtHash) { throw 'Packaged executable differs from the build' }
    $help = & $exe --help
    if ($LASTEXITCODE -ne 0 -or ($help -join "`n") -notmatch 'Usage:') { throw 'Packaged executable failed --help' }
    $gitVersion = & $exe git --version
    if ($LASTEXITCODE -ne 0 -or ($gitVersion -join "`n") -notmatch '^git version') { throw "Packaged 'github-turbo git --version' failed: $gitVersion" }
    Write-Host "Smoke: --help ok; git wrapper ok ($gitVersion)"
    if ($NetworkSmoke) {
        $env:GIT_TERMINAL_PROMPT = '0'
        $refs = & $exe git ls-remote https://github.com/octocat/Hello-World HEAD
        if ($LASTEXITCODE -ne 0 -or ($refs -join "`n") -notmatch '\bHEAD$') { throw 'Network smoke (git ls-remote through the wrapper) failed' }
        Write-Host 'Smoke: git ls-remote through the wrapper ok'
    }
    Remove-Item -LiteralPath $verify -Recurse -Force

    # --- Outputs ---------------------------------------------------------------
    $digest = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
    [IO.File]::WriteAllText("$archive.sha256", "$digest  $name.zip`n", [Text.Encoding]::ASCII)
    [IO.File]::WriteAllText((Join-Path $rcDir "$name.json"), $manifestJson, (New-Object Text.UTF8Encoding $false))
    Get-Item -LiteralPath $archive | Select-Object FullName, Length
    Write-Output "SHA256 $digest"
    Write-Output "Commit $commit$(if ($dirty) { ' (dirty)' })"
} finally { Pop-Location }
