param()
$ErrorActionPreference='Stop'
$root=Split-Path $PSScriptRoot -Parent
$version=((Get-Content -Raw (Join-Path $root 'Cargo.toml')) | Select-String '(?m)^version = "([^"]+)"').Matches[0].Groups[1].Value
$stage=Join-Path $root ("packages\staging\GitHubTurbo-Benchmark-$version")
foreach($dir in @('bench','bench\binaries','dist')) { New-Item -ItemType Directory -Force -Path (Join-Path $stage $dir) | Out-Null }
foreach($file in @('bench.py','index.html','cases.json','README.md','Start.cmd','test_bench.py','check_platform.py','ip_probe.py','ip-http2.js')) {
    Copy-Item -LiteralPath (Join-Path $root ('bench\'+$file)) -Destination (Join-Path $stage 'bench')
}
foreach($file in @('baseline.exe','baseline.json','baseline.config.json')) {
    Copy-Item -LiteralPath (Join-Path $root ('bench\binaries\'+$file)) -Destination (Join-Path $stage 'bench\binaries')
}
foreach($file in @('v0.4.1.exe','v0.4.1.config.json')) {
    $source=Join-Path $root ('bench\binaries\'+$file)
    if(Test-Path -LiteralPath $source) { Copy-Item -LiteralPath $source -Destination (Join-Path $stage 'bench\binaries') }
}
foreach($file in @('github-turbo.exe','windows.ps1','Start.cmd','Stop.cmd','Recover.cmd','dependencies.json')) {
    Copy-Item -LiteralPath (Join-Path $root ('dist\'+$file)) -Destination (Join-Path $stage 'dist')
}
foreach($file in @('LICENSE','README.md','config.json')) { Copy-Item -LiteralPath (Join-Path $root $file) -Destination $stage }
Copy-Item -LiteralPath (Join-Path $root 'config.json') -Destination (Join-Path $stage 'dist\config.json')
Copy-Item -LiteralPath (Join-Path $root 'third-party') -Destination $stage -Recurse -Force
Copy-Item -LiteralPath (Join-Path $root 'dist\licenses') -Destination (Join-Path $stage 'dist') -Recurse -Force
New-Item -ItemType Directory -Force -Path (Join-Path $stage 'docs') | Out-Null
Copy-Item -LiteralPath (Join-Path $root 'docs\validation-0.4.md') -Destination (Join-Path $stage 'docs')
Copy-Item -LiteralPath (Join-Path $root 'docs\release-regression-0.4.1.md') -Destination (Join-Path $stage 'docs')
Copy-Item -LiteralPath (Join-Path $root 'docs\ip-selection-fgit.md') -Destination (Join-Path $stage 'docs')
Copy-Item -LiteralPath (Join-Path $root 'docs\routing-0.5.md') -Destination (Join-Path $stage 'docs')
# Share report files only; test CA keys, repos, response headers and bodies stay on this machine.
$runs=Get-ChildItem -LiteralPath (Join-Path $root 'bench\runs') -Directory
foreach($run in $runs) {
    $reportPath=Join-Path $run.FullName 'report.json'
    if(!(Test-Path -LiteralPath $reportPath)) { continue }
    try { $report=Get-Content -LiteralPath $reportPath -Raw -Encoding UTF8 | ConvertFrom-Json }
    catch { throw ('Cannot parse UTF-8 benchmark report: '+$run.Name) }
    if($report.state -ne 'completed') { continue }
    $destination=Join-Path $stage ('bench\runs\'+$run.Name)
    New-Item -ItemType Directory -Force -Path $destination | Out-Null
    foreach($file in @('report.json','report.html')) {
        $source=Join-Path $run.FullName $file
        if(Test-Path -LiteralPath $source) { Copy-Item -LiteralPath $source -Destination $destination }
    }
}
[IO.File]::WriteAllText((Join-Path $stage 'Start-Benchmark.cmd'),('@echo off'+"`r`n"+'call "%~dp0bench\Start.cmd"'+"`r`n"),[Text.Encoding]::ASCII)
Get-ChildItem -LiteralPath $stage -Recurse -File | Where-Object {$_.LastWriteTimeUtc -lt [datetime]'1981-01-01'} | ForEach-Object {$_.LastWriteTimeUtc=[datetime]'2000-01-01'}
$archive=Join-Path $root ("packages\GitHubTurbo-Benchmark-$version.zip")
Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $archive -Force
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip=[IO.Compression.ZipFile]::OpenRead($archive)
try {
    foreach($entry in $zip.Entries) {
        $name=$entry.FullName -replace '\\','/'
        if($name -match '(^|/)(state|private-[^/]+|\.git|work)/|\.(pem|body|headers|log)$|session\.json') { throw ('Private test state was packaged: '+$name) }
    }
} finally { $zip.Dispose() }
$hash=(Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText(($archive+'.sha256'),($hash+'  '+[IO.Path]::GetFileName($archive)+"`n"),[Text.Encoding]::ASCII)
Get-Item -LiteralPath $archive | Select-Object FullName,Length
Write-Output "SHA256 $hash"
