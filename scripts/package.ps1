param([switch]$SkipBuild)
$ErrorActionPreference='Stop'
$root=Split-Path $PSScriptRoot -Parent
if (!$SkipBuild) { & (Join-Path $PSScriptRoot 'build.ps1') }
$version = ((Get-Content -Raw (Join-Path $root 'Cargo.toml')) | Select-String '(?m)^version = "([^"]+)"').Matches[0].Groups[1].Value
$stage=Join-Path $root ("packages\staging\GitHubTurbo-$version-windows-x64")
New-Item -ItemType Directory -Force -Path $stage | Out-Null
foreach ($name in @('github-turbo.exe','windows.ps1','Start.cmd','Stop.cmd','Recover.cmd','README.md','LICENSE','dependencies.json')) { Copy-Item -LiteralPath (Join-Path $root ('dist\'+$name)) -Destination $stage }
Copy-Item -LiteralPath (Join-Path $root 'third-party') -Destination $stage -Recurse -Force
# Use the checked-in default configuration, never the active machine's settings.
Copy-Item -LiteralPath (Join-Path $root 'config.json') -Destination (Join-Path $stage 'config.json')
Copy-Item -LiteralPath (Join-Path $root 'dist\licenses') -Destination $stage -Recurse -Force
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'scripts') | Out-Null
Copy-Item -LiteralPath (Join-Path $root 'scripts\download.ps1') -Destination (Join-Path $stage 'scripts\download.ps1')
Copy-Item -LiteralPath (Join-Path $root 'scripts\hosts.ps1') -Destination (Join-Path $stage 'scripts\hosts.ps1')
Copy-Item -LiteralPath (Join-Path $root 'dist\docs') -Destination $stage -Recurse -Force
$archive=Join-Path $root ("packages\GitHubTurbo-$version-windows-x64.zip")
Get-ChildItem -LiteralPath $stage -Recurse -File | Where-Object { $_.LastWriteTimeUtc -lt [datetime]'1981-01-01' } | ForEach-Object { $_.LastWriteTimeUtc=[datetime]'2000-01-01' }
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $archive -Force
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip=[IO.Compression.ZipFile]::OpenRead($archive)
try {
    $names=@($zip.Entries | ForEach-Object { $_.FullName -replace '\\','/' })
    if ($names | Where-Object { $_ -match '(^|/)(state|reports|\.git)/|ca-key\.pem|server-key\.pem|session\.json' }) { throw 'Private machine state was included in the archive' }
    if (!($names -contains 'github-turbo.exe') -or !($names -contains 'Start.cmd')) { throw 'Incomplete archive' }
} finally { $zip.Dispose() }
$digest=(Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText(($archive+'.sha256'),($digest+'  '+[IO.Path]::GetFileName($archive)+"`n"),[Text.Encoding]::ASCII)
Get-Item -LiteralPath $archive | Select-Object FullName,Length
Write-Output "SHA256 $digest"
