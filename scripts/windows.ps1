param([ValidateSet('enable','disable','recover','watch')][string]$Action = 'enable', [int]$OwnerPid = 0, [switch]$NoBrowser)
$ErrorActionPreference = 'Stop'
$base = Split-Path -Parent $MyInvocation.MyCommand.Path
$exe = Join-Path $base 'github-turbo.exe'
$configPath = Join-Path $base 'config.json'
try { $cfg = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json }
catch { if ($Action -eq 'enable') { throw 'Configuration is invalid. Recover.cmd remains available to restore an active default session.' }; $cfg=[pscustomobject]@{transparent=[pscustomobject]@{state_dir='state'}} }
$stateDir = if ([IO.Path]::IsPathRooted($cfg.transparent.state_dir)) { [IO.Path]::GetFullPath($cfg.transparent.state_dir) } else { [IO.Path]::GetFullPath((Join-Path $base $cfg.transparent.state_dir)) }
$sessionPath = Join-Path $stateDir 'session.json'
$hostsPath = Join-Path $env:SystemRoot 'System32\drivers\etc\hosts'
$prefix = '# GitHubTurbo saved: '
$begin = '# GitHubTurbo managed BEGIN'
$end = '# GitHubTurbo managed END'
$domains = @('github.com','githubusercontent.com','githubassets.com','github.io','githubapp.com','github-cloud.s3.amazonaws.com','github-com.s3.amazonaws.com','github-production-release-asset-2e65be.s3.amazonaws.com','github-production-repository-file-5c1aeb.s3.amazonaws.com','github-production-user-asset-6210df.s3.amazonaws.com')
function Is-Managed([string]$name) { foreach ($domain in $domains) { if ($name -eq $domain -or $name.EndsWith('.' + $domain,[StringComparison]::OrdinalIgnoreCase)) { return $true } }; return $false }
function Write-Session($value) { $json = $value | ConvertTo-Json -Depth 10; $temp = $sessionPath + '.tmp'; [IO.File]::WriteAllText($temp,$json,[Text.UTF8Encoding]::new($false)); Move-Item -LiteralPath $temp -Destination $sessionPath -Force }
function Restore-Hosts([string]$text) {
    $inside = $false
    $rows = foreach ($line in ($text -split "`r?`n")) {
        if ($line -eq $begin) { $inside = $true; continue }
        if ($line -eq $end) { $inside = $false; continue }
        if ($inside -or $line.EndsWith(' # GitHubTurbo retained')) { continue }
        if ($line.StartsWith($prefix)) { [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($line.Substring($prefix.Length))); continue }
        $line
    }
    return ($rows -join "`r`n")
}
function Restore-Session {
    if (!(Test-Path -LiteralPath $sessionPath)) { return }
    $session = Get-Content -LiteralPath $sessionPath -Raw | ConvertFrom-Json
    if ($session.recoveryTask -and $session.recoveryTask.StartsWith('GitHubTurboRecovery-')) { Unregister-ScheduledTask -TaskName $session.recoveryTask -Confirm:$false -ErrorAction SilentlyContinue }
    # Stop routing first; only remove rules created by this application.
    Get-DnsClientNrptRule | Where-Object { $_.Comment -eq $session.tag } | Remove-DnsClientNrptRule -Force
    $current = [IO.File]::ReadAllBytes($hostsPath)
    $hash = [Convert]::ToBase64String([Security.Cryptography.SHA256]::Create().ComputeHash($current))
    if ($session.hostsAppliedHash) {
        if ($hash -eq $session.hostsAppliedHash) { [IO.File]::WriteAllBytes($hostsPath,[Convert]::FromBase64String($session.hostsOriginal)) }
        else { [IO.File]::WriteAllText($hostsPath,(Restore-Hosts ([IO.File]::ReadAllText($hostsPath))),[Text.UTF8Encoding]::new($false)) }
    }
    foreach ($entry in $session.git) {
        $present = @(& git config --global --get-all $entry.key 2>$null)
        if (($present -join "`n") -eq ($entry.applied -join "`n")) {
            & git config --global --unset-all $entry.key 2>$null
            foreach ($value in $entry.previous) { & git config --global --add $entry.key $value; if ($LASTEXITCODE -ne 0) { throw 'Unable to restore Git configuration' } }
        }
    }
    if ($session.ssh) {
        $sshPath = $session.ssh.path
        if (Test-Path -LiteralPath $sshPath) {
            $text = [IO.File]::ReadAllText($sshPath)
            if ($text.StartsWith($session.ssh.block)) {
                $remaining = $text.Substring($session.ssh.block.Length)
                if (!$session.ssh.existed -and $remaining.Length -eq 0) { Remove-Item -LiteralPath $sshPath }
                elseif ($session.ssh.originalBytes -and $text -eq ($session.ssh.block+[Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($session.ssh.originalBytes)))) { [IO.File]::WriteAllBytes($sshPath,[Convert]::FromBase64String($session.ssh.originalBytes)) }
                else { [IO.File]::WriteAllText($sshPath,$remaining,[Text.UTF8Encoding]::new($false)) }
            }
        }
    }
    if ($session.certAdded) { $store = if ($session.certStore) { $session.certStore } else { 'CurrentUser' }; $certPath = 'Cert:\' + $store + '\Root\' + $session.thumbprint; if (Test-Path $certPath) { Remove-Item -LiteralPath $certPath } }
    if ($session.enginePid) {
        $engine = Get-Process -Id $session.enginePid -ErrorAction SilentlyContinue
        if ($engine -and $engine.Path -eq $exe -and $engine.StartTime.ToUniversalTime().Ticks.ToString() -eq $session.engineTicks) { Stop-Process -Id $engine.Id }
    }
    Clear-DnsClientCache
    Remove-Item -LiteralPath $sessionPath
}
$admin = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
if (!$admin) {
    if ($Action -eq 'watch') { throw 'Watchdog requires administrator privileges' }
    $child = Start-Process powershell.exe -Verb RunAs -ArgumentList "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Action $Action" -PassThru -Wait
    exit $child.ExitCode
}
if ($Action -eq 'watch') {
    while (Test-Path -LiteralPath $sessionPath) {
        $session = Get-Content -Raw -LiteralPath $sessionPath | ConvertFrom-Json
        if ($session.enginePid -ne $OwnerPid) { exit 0 }
        $engine = Get-Process -Id $OwnerPid -ErrorAction SilentlyContinue
        if (!$engine -or $engine.Path -ne $exe -or $engine.StartTime.ToUniversalTime().Ticks.ToString() -ne $session.engineTicks) {
            $guard = [Threading.Mutex]::new($false,'Local\GitHubTurboIntegration'); $acquired=$false
            try { try { $acquired=$guard.WaitOne(10000) } catch [Threading.AbandonedMutexException] { $acquired=$true }; if ($acquired -and (Test-Path -LiteralPath $sessionPath)) { $latest=Get-Content -Raw -LiteralPath $sessionPath | ConvertFrom-Json; if ($latest.enginePid -eq $OwnerPid) { Restore-Session } } }
            finally { if ($acquired) { $guard.ReleaseMutex() }; $guard.Dispose() }; exit 0
        }
        Start-Sleep -Seconds 2
    }
    exit 0
}
# Serialize all integration changes, including an abandoned launcher recovery.
$mutex = [Threading.Mutex]::new($false,'Local\GitHubTurboIntegration')
$locked = $false
try {
    try { $locked = $mutex.WaitOne(10000) } catch [Threading.AbandonedMutexException] { $locked = $true }
    if (!$locked) { throw 'Another GitHubTurbo operation is in progress' }
    if ($Action -ne 'enable') {
        if ($OwnerPid -gt 0 -and (Test-Path -LiteralPath $sessionPath)) { $current=Get-Content -Raw -LiteralPath $sessionPath | ConvertFrom-Json; if ($current.enginePid -ne $OwnerPid) { exit 0 } }
        Restore-Session; Write-Output 'GitHubTurbo disabled; original settings restored.'; exit 0
    }
    if (Test-Path -LiteralPath $sessionPath) {
        $session = Get-Content -Raw -LiteralPath $sessionPath | ConvertFrom-Json
        $engine = Get-Process -Id $session.enginePid -ErrorAction SilentlyContinue
        if ($engine -and $engine.Path -eq $exe -and $engine.StartTime.ToUniversalTime().Ticks.ToString() -eq $session.engineTicks) { Write-Output 'GitHubTurbo is already enabled.'; if (!$NoBrowser) { Start-Process "http://$($cfg.listen)/" }; exit 0 }
        Restore-Session
    }
    if (!$cfg.transparent.enabled) { throw 'transparent.enabled must be true' }
    if ($cfg.transparent.https_listen -ne '127.0.0.1:443' -or $cfg.transparent.http_listen -ne '127.0.0.1:80' -or $cfg.transparent.dns_listen -ne '127.0.0.1:53') { throw 'System integration requires the standard loopback HTTPS, HTTP and DNS ports' }
    foreach ($address in @($cfg.listen,$cfg.transparent.https_listen,$cfg.transparent.http_listen,$cfg.transparent.dns_listen)) {
        $port = [int]($address -replace '^.*:', '')
        if (Get-NetTCPConnection -State Listen -LocalPort $port -ErrorAction SilentlyContinue) { throw "Port $port is already occupied; stop the conflicting application first" }
    }
    if (Get-NetUDPEndpoint -LocalPort ([int]($cfg.transparent.dns_listen -replace '^.*:', '')) -ErrorAction SilentlyContinue | Where-Object { $_.LocalAddress -in @('0.0.0.0','127.0.0.1') }) { throw 'Local DNS UDP port is occupied' }
    New-Item -ItemType Directory -Force -Path $stateDir | Out-Null
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    & icacls.exe $stateDir /inheritance:r /grant:r "*$($sid):(OI)(CI)F" '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Unable to protect private certificate state' }
    & $exe init-ca $configPath
    if ($LASTEXITCODE -ne 0) { throw 'Certificate initialization failed' }
    $ca = [Security.Cryptography.X509Certificates.X509Certificate2]::new((Join-Path $stateDir 'ca.pem'))
    $certAdded = !(Test-Path ('Cert:\LocalMachine\Root\' + $ca.Thumbprint))
    $original = [IO.File]::ReadAllBytes($hostsPath)
    $session = [ordered]@{ tag=('GitHubTurbo ' + [guid]::NewGuid().ToString()); hostsOriginal=[Convert]::ToBase64String($original); hostsAppliedHash=$null; certAdded=$certAdded; certStore='LocalMachine'; thumbprint=$ca.Thumbprint; git=@(); ssh=$null; recoveryTask=$null; enginePid=0; engineTicks=''; phase='preparing' }
    Write-Session $session
    try {
        if ($certAdded) { Import-Certificate -FilePath (Join-Path $stateDir 'ca.pem') -CertStoreLocation Cert:\LocalMachine\Root | Out-Null }
        if (Get-Command git -ErrorAction SilentlyContinue) {
            foreach ($hostPattern in @('github.com','*.github.com','*.githubusercontent.com')) { foreach ($setting in @('sslCAInfo','sslVerify')) {
                $key = "http.https://$hostPattern/.$setting"
                $previous = @(& git config --global --get-all $key 2>$null)
                $value = if ($setting -eq 'sslVerify') { 'true' } else { (Join-Path $stateDir 'ca.pem') -replace '\\','/' }
                $session.git += [ordered]@{key=$key;previous=$previous;applied=@($value)}; Write-Session $session
                & git config --global --replace-all $key $value
                if ($LASTEXITCODE -ne 0) { throw 'Unable to configure Git CA trust' }
            }}
        }
        $sshDir = Join-Path ([Environment]::GetFolderPath('UserProfile')) '.ssh'
        New-Item -ItemType Directory -Force -Path $sshDir | Out-Null
        $sshPath = Join-Path $sshDir 'config'
        $sshExisted = Test-Path -LiteralPath $sshPath
        $sshOriginal = if ($sshExisted) { [IO.File]::ReadAllText($sshPath) } else { '' }
        $sshBlock = "# GitHubTurbo BEGIN`r`nHost github.com ssh.github.com`r`n    ProxyCommand `"$exe`" ssh-tunnel`r`nHost *`r`n# GitHubTurbo END`r`n"
        $sshBytes = if ($sshExisted) { [Convert]::ToBase64String([IO.File]::ReadAllBytes($sshPath)) } else { '' }
        $session.ssh = [ordered]@{path=$sshPath;existed=$sshExisted;block=$sshBlock;originalBytes=$sshBytes};Write-Session $session
        [IO.File]::WriteAllText($sshPath,($sshBlock+$sshOriginal),[Text.UTF8Encoding]::new($false))
        $engine = Start-Process -FilePath $exe -ArgumentList 'serve' -WorkingDirectory $base -WindowStyle Hidden -RedirectStandardError (Join-Path $stateDir 'engine.log') -RedirectStandardOutput (Join-Path $stateDir 'engine-out.log') -PassThru
        $session.enginePid=$engine.Id; $session.engineTicks=$engine.StartTime.ToUniversalTime().Ticks.ToString(); Write-Session $session
        $ready = $false
        for ($attempt=0; $attempt -lt 30; $attempt++) {
            if ($engine.HasExited) { throw 'Engine exited; see state/engine.log' }
            try { $status = Invoke-RestMethod -Uri "http://$($cfg.listen)/status" -TimeoutSec 1; if ($status.version -like '0.3.*') { $ready=$true; break } } catch {}
            Start-Sleep -Milliseconds 100
        }
        if (!$ready) { throw 'Engine did not become ready' }
        $session.recoveryTask='GitHubTurboRecovery-'+[guid]::NewGuid().ToString();Write-Session $session
        $recoveryAction=New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$PSCommandPath`" -Action recover -OwnerPid $($engine.Id)"
        $recoveryTrigger=New-ScheduledTaskTrigger -AtLogOn -User $sid
        $recoveryPrincipal=New-ScheduledTaskPrincipal -UserId $sid -LogonType Interactive -RunLevel Highest
        Register-ScheduledTask -TaskName $session.recoveryTask -Action $recoveryAction -Trigger $recoveryTrigger -Principal $recoveryPrincipal -Description 'Restore GitHubTurbo network settings after an interrupted session; removed on normal stop.' | Out-Null
        # Existing explicit hosts take precedence over NRPT: replace their GitHub entries reversibly.
        $known = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        foreach ($hostName in @('github.com','www.github.com','api.github.com','raw.githubusercontent.com','codeload.github.com','release-assets.githubusercontent.com','objects.githubusercontent.com','github.githubassets.com','avatars.githubusercontent.com','media.githubusercontent.com','gist.github.com','gist.githubusercontent.com','user-images.githubusercontent.com','private-user-images.githubusercontent.com','desktop.githubusercontent.com','ssh.github.com')) { [void]$known.Add($hostName) }
        $rows = foreach ($line in ([Text.Encoding]::UTF8.GetString($original) -split "`r?`n")) {
            $tokens = @(($line -split '#',2)[0].Trim() -split '\s+' | Where-Object { $_ })
            $managed = @(); $other = @()
            if ($tokens.Count -gt 1) { foreach ($hostName in $tokens[1..($tokens.Count-1)]) { if (Is-Managed $hostName) { $managed += $hostName; [void]$known.Add($hostName) } else { $other += $hostName } } }
            if ($managed.Count) {
                $prefix + [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($line))
                if ($other.Count) { $tokens[0] + ' ' + ($other -join ' ') + ' # GitHubTurbo retained' }
            } else { $line }
        }
        $rows += $begin
        foreach ($hostName in $known) { $rows += '127.0.0.1 ' + $hostName }
        $rows += $end
        $patched = [Text.UTF8Encoding]::new($false).GetBytes(($rows -join "`r`n"))
        $session.hostsAppliedHash=[Convert]::ToBase64String([Security.Cryptography.SHA256]::Create().ComputeHash($patched));Write-Session $session
        [IO.File]::WriteAllBytes($hostsPath,$patched)
        foreach ($domain in $domains) { Add-DnsClientNrptRule -Namespace @($domain,('.'+$domain)) -NameServers '127.0.0.1' -Comment $session.tag | Out-Null }
        Clear-DnsClientCache
        $session.phase='enabled';Write-Session $session
        $watcher = Start-Process -FilePath $exe -ArgumentList @('watch',$engine.Id) -WorkingDirectory $base -WindowStyle Hidden -RedirectStandardError (Join-Path $stateDir 'watchdog.log') -PassThru
        Start-Sleep -Milliseconds 150
        if ($watcher.HasExited) { throw 'Recovery watchdog failed to start' }
        Write-Output "GitHubTurbo enabled: http://$($cfg.listen)/"
        if (!$NoBrowser) { Start-Process "http://$($cfg.listen)/" }
    } catch { $failure=$_; Restore-Session; throw $failure }
} finally { if ($locked) { $mutex.ReleaseMutex() }; $mutex.Dispose() }
