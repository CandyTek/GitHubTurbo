# Exercises actual Windows integration. Run as administrator; leaves acceleration enabled.
$ErrorActionPreference='Stop'
$root=Split-Path $PSScriptRoot -Parent
$launcher=Join-Path $root 'dist\windows.ps1'
$sessionPath=Join-Path $root 'dist\state\session.json'
$hostsPath=Join-Path $env:SystemRoot 'System32\drivers\etc\hosts'
function Assert-Restored($previous) {
    if (Test-Path -LiteralPath $sessionPath) { throw 'Session was not removed' }
    $expected=[Security.Cryptography.SHA256]::Create().ComputeHash([Convert]::FromBase64String($previous.hostsOriginal))
    $actual=[Security.Cryptography.SHA256]::Create().ComputeHash([IO.File]::ReadAllBytes($hostsPath))
    if ([Convert]::ToBase64String($expected) -ne [Convert]::ToBase64String($actual)) { throw 'Hosts restoration was not byte-exact' }
    if (Get-DnsClientNrptRule | Where-Object Comment -eq $previous.tag) { throw 'Owned NRPT rules remain' }
    foreach ($entry in $previous.git) { $value=@(& git config --global --get-all $entry.key 2>$null); if (($value -join "`n") -ne ($entry.previous -join "`n")) { throw ('Git setting was not restored: '+$entry.key) } }
    if ($previous.certAdded -and (Test-Path ('Cert:\'+$previous.certStore+'\Root\'+$previous.thumbprint))) { throw 'Owned CA trust remains' }
    if ($previous.recoveryTask -and (Get-ScheduledTask -TaskName $previous.recoveryTask -ErrorAction SilentlyContinue)) { throw 'Owned recovery task remains' }
    if ($previous.ssh -and $previous.ssh.originalBytes) { $actual=[Convert]::ToBase64String([IO.File]::ReadAllBytes($previous.ssh.path));if ($actual -ne $previous.ssh.originalBytes) { throw 'SSH config restoration was not byte-exact' } }
}
if (!(Test-Path -LiteralPath $sessionPath)) { & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $launcher -Action enable -NoBrowser; if ($LASTEXITCODE -ne 0) { throw 'Enable failed' } }
$before=Get-Content -Raw -LiteralPath $sessionPath | ConvertFrom-Json
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $launcher -Action disable
if ($LASTEXITCODE -ne 0) { throw 'Disable failed' }
Assert-Restored $before
Write-Output 'PASS: normal stop restores hosts, scoped Git settings, CA, NRPT, recovery task'
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $launcher -Action enable -NoBrowser
if ($LASTEXITCODE -ne 0) { throw 'Re-enable failed' }
$before=Get-Content -Raw -LiteralPath $sessionPath | ConvertFrom-Json
$engine=Get-Process -Id $before.enginePid
if ($engine.Path -ne (Join-Path $root 'dist\github-turbo.exe')) { throw 'Unexpected engine identity' }
Stop-Process -Id $engine.Id
for ($attempt=0;$attempt -lt 80;$attempt++) { if (!(Test-Path -LiteralPath $sessionPath)) { break }; Start-Sleep -Milliseconds 250 }
Assert-Restored $before
Write-Output 'PASS: forced engine termination triggers automatic recovery'
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $launcher -Action enable -NoBrowser
if ($LASTEXITCODE -ne 0) { throw 'Final enable failed' }
$before=Get-Content -Raw -LiteralPath $sessionPath | ConvertFrom-Json
$configPath=Join-Path $root 'dist\config.json'
$configBytes=[IO.File]::ReadAllBytes($configPath)
try {
    [IO.File]::WriteAllText($configPath,'{invalid JSON')
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $launcher -Action recover
    if ($LASTEXITCODE -ne 0) { throw 'Recovery with invalid configuration failed' }
    Assert-Restored $before
} finally { [IO.File]::WriteAllBytes($configPath,$configBytes) }
Write-Output 'PASS: recovery works even when config.json is malformed'
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $launcher -Action enable -NoBrowser
if ($LASTEXITCODE -ne 0) { throw 'Final enable failed' }
@{normal_stop='passed';crash_recovery='passed';invalid_config_recovery='passed';hosts='byte-exact';ssh='byte-exact';git='restored';nrpt='removed';ca='removed';recovery_task='removed';final='enabled'} | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $root 'reports\lifecycle-0.3.json')
