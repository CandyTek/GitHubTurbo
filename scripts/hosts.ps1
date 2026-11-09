param(
    [ValidateSet('inspect','clean','restore')][string]$Action='inspect',
    [string]$HostsPath=(Join-Path $env:SystemRoot 'System32\drivers\etc\hosts'),
    [string]$BackupDir=(Join-Path (Split-Path $PSScriptRoot -Parent) 'state\hosts-backups'),
    [string]$Backup
)
$ErrorActionPreference='Stop'
function Digest([byte[]]$bytes) {
    $sha=[Security.Cryptography.SHA256]::Create()
    try { ([BitConverter]::ToString($sha.ComputeHash($bytes))).Replace('-','').ToLowerInvariant() } finally { $sha.Dispose() }
}
# Latin-1 round trips arbitrary bytes. Only ASCII mappings and known block markers
# are edited, so unrelated UTF-8/ANSI text, BOMs and newline bytes remain intact.
$encoding=[Text.Encoding]::GetEncoding(28591)
$original=[IO.File]::ReadAllBytes($HostsPath)
if ($Action -eq 'restore') {
    if (!$Backup) { throw 'Specify -Backup with the backup manifest path' }
    $manifest=Get-Content -Raw -LiteralPath $Backup | ConvertFrom-Json
    if ([IO.Path]::GetFullPath($HostsPath) -ne $manifest.path) { throw 'Backup belongs to a different hosts path' }
    if ((Digest $original) -ne $manifest.cleaned_sha256) { throw 'Hosts changed after cleaning; restore refused to preserve later edits' }
    $bytes=[IO.File]::ReadAllBytes([IO.Path]::ChangeExtension($Backup,'.bin'))
    if ((Digest $bytes) -ne $manifest.original_sha256) { throw 'Backup checksum mismatch' }
    [IO.File]::WriteAllBytes($HostsPath,$bytes)
    if ($HostsPath -eq (Join-Path $env:SystemRoot 'System32\drivers\etc\hosts')) { Clear-DnsClientCache }
    Write-Output 'Hosts restored byte for byte'
    return
}
$output=New-Object Text.StringBuilder
$inside=$false
$removed=0
$scope='(?i)^(?:[a-z0-9-]+\.)*(?:github\.com|githubusercontent\.com|githubassets\.com|github\.io|githubapp\.com|githubcopilot\.com|githubstatus\.com|github\.blog|github\.community|github\.global\.ssl\.fastly\.net|github\.map\.fastly\.net|github-com\.s3\.amazonaws\.com|github-cloud\.s3\.amazonaws\.com|github-production-[a-z0-9-]+\.s3\.amazonaws\.com)\.?$'
foreach ($line in [regex]::Split($encoding.GetString($original),'(?<=\n)')) {
    if ($line -match '(?i)^\s*#\s*(GitHub520 Host|GithubHosts|GitHubTurbo).*Start') { $inside=$true; $removed++; continue }
    if ($line -match '(?i)^\s*#\s*(GitHub520 Host|GithubHosts|GitHubTurbo).*End') { $inside=$false; $removed++; continue }
    if ($inside) { $removed++; continue }
    $main=($line -split '#',2)[0]
    $tokens=@([regex]::Matches($main,'\S+') | ForEach-Object { $_.Value })
    if ($tokens.Count -lt 2) { [void]$output.Append($line); continue }
    $ip=$null
    if (![Net.IPAddress]::TryParse($tokens[0],[ref]$ip)) { [void]$output.Append($line); continue }
    $kept=@($tokens[1..($tokens.Count-1)] | Where-Object { $_ -notmatch $scope })
    if ($kept.Count -eq $tokens.Count-1) { [void]$output.Append($line); continue }
    $removed++
    if ($kept.Count -gt 0) {
        $tail=if ($line.Contains('#')) { ' '+$line.Substring($line.IndexOf('#')).TrimEnd("`r","`n") } else { '' }
        $newline=if ($line.EndsWith("`r`n")) { "`r`n" } elseif ($line.EndsWith("`n")) { "`n" } else { '' }
        [void]$output.Append($tokens[0]+"`t"+($kept -join ' ')+$tail+$newline)
    }
}
if ($inside) { throw 'Unclosed accelerator hosts block; no changes were written' }
$cleaned=$encoding.GetBytes($output.ToString())
if ($Action -eq 'inspect') { [pscustomobject]@{path=$HostsPath;changed_lines=$removed;before_bytes=$original.Length;after_bytes=$cleaned.Length}; return }
if ((Digest $original) -eq (Digest $cleaned)) { Write-Output 'No stale GitHub mappings found'; return }
New-Item -ItemType Directory -Force -Path $BackupDir | Out-Null
$stem=Join-Path $BackupDir ((Get-Date -Format 'yyyyMMdd-HHmmss-fff')+'-'+[guid]::NewGuid().ToString('N').Substring(0,8))
[IO.File]::WriteAllBytes(($stem+'.bin'),$original)
$manifest=@{path=[IO.Path]::GetFullPath($HostsPath);utc=[datetime]::UtcNow.ToString('o');original_sha256=(Digest $original);cleaned_sha256=(Digest $cleaned);changed_lines=$removed}
$manifest | ConvertTo-Json | Set-Content -LiteralPath ($stem+'.json') -Encoding UTF8
if ((Digest ([IO.File]::ReadAllBytes($HostsPath))) -ne $manifest.original_sha256) { throw 'Hosts changed during cleanup; no changes were written' }
[IO.File]::WriteAllBytes($HostsPath,$cleaned)
if ($HostsPath -eq (Join-Path $env:SystemRoot 'System32\drivers\etc\hosts')) { Clear-DnsClientCache }
[pscustomobject]@{changed_lines=$removed;backup=($stem+'.json');cleaned_sha256=$manifest.cleaned_sha256}
