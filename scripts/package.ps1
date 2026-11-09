param([switch]$SkipBuild, [switch]$Overwrite)
$ErrorActionPreference='Stop'
$root=Split-Path $PSScriptRoot -Parent
$version = ((Get-Content -Raw (Join-Path $root 'Cargo.toml')) | Select-String '(?m)^version = "([^"]+)"').Matches[0].Groups[1].Value
$archive=Join-Path $root ("packages\GitHubTurbo-$version-windows-x64.zip")
# A published package must never change under the same name. Check before building.
if ((Test-Path -LiteralPath $archive) -and !$Overwrite) {
    throw "$archive already exists. Bump the version in Cargo.toml, or pass -Overwrite if this version was never published."
}
if (!$SkipBuild) { & (Join-Path $PSScriptRoot 'build.ps1') }
$stage=Join-Path $root ("packages\staging\GitHubTurbo-$version-windows-x64")
# Start from an empty stage so files from an earlier run cannot slip in.
if (Test-Path -LiteralPath $stage) { Remove-Item -LiteralPath $stage -Recurse -Force }
New-Item -ItemType Directory -Force -Path $stage | Out-Null
foreach ($name in @('github-turbo.exe','windows.ps1','Start.cmd','Stop.cmd','Recover.cmd','README.md','README_zh.md','LICENSE','dependencies.json')) { Copy-Item -LiteralPath (Join-Path $root ('dist\'+$name)) -Destination $stage }
Copy-Item -LiteralPath (Join-Path $root 'third-party') -Destination $stage -Recurse -Force
# Use the checked-in default configuration, never the active machine's settings.
Copy-Item -LiteralPath (Join-Path $root 'config.json') -Destination (Join-Path $stage 'config.json')
Copy-Item -LiteralPath (Join-Path $root 'dist\licenses') -Destination $stage -Recurse -Force
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'scripts') | Out-Null
Copy-Item -LiteralPath (Join-Path $root 'scripts\download.ps1') -Destination (Join-Path $stage 'scripts\download.ps1')
Copy-Item -LiteralPath (Join-Path $root 'scripts\hosts.ps1') -Destination (Join-Path $stage 'scripts\hosts.ps1')
Copy-Item -LiteralPath (Join-Path $root 'dist\docs') -Destination $stage -Recurse -Force
Get-ChildItem -LiteralPath $stage -Recurse -File | Where-Object { $_.LastWriteTimeUtc -lt [datetime]'1981-01-01' } | ForEach-Object { $_.LastWriteTimeUtc=[datetime]'2000-01-01' }
if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive -Force }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $archive
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip=[IO.Compression.ZipFile]::OpenRead($archive)
try {
    $names=@($zip.Entries | ForEach-Object { $_.FullName -replace '\\','/' })
    if ($names | Where-Object { $_ -match '(^|/)(state|reports|\.git)/|ca-key\.pem|server-key\.pem|session\.json' }) { throw 'Private machine state was included in the archive' }
    foreach ($required in @('github-turbo.exe','Start.cmd','README.md','README_zh.md','LICENSE','dependencies.json')) {
        if (!($names -contains $required)) { throw "Incomplete archive: $required is missing" }
    }
} finally { $zip.Dispose() }
$digest=(Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText(($archive+'.sha256'),($digest+'  '+[IO.Path]::GetFileName($archive)+"`n"),[Text.Encoding]::ASCII)
Get-Item -LiteralPath $archive | Select-Object FullName,Length
Write-Output "SHA256 $digest"
