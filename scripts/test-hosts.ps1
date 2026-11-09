$ErrorActionPreference='Stop'
$dir=Join-Path ([IO.Path]::GetTempPath()) ('turbo-hosts-'+[guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $dir | Out-Null
$file=Join-Path $dir 'hosts'
$script=Join-Path $PSScriptRoot 'hosts.ps1'
try {
    $text="# unchanged`r`n127.0.0.1 local.test`r`n1.1.1.1 github.com local.alias # mixed`r`n# GitHub520 Host Start`r`n8.8.8.8 raw.githubusercontent.com`r`n# GitHub520 Host End`r`n8.8.8.8 github.com.evil.test`r`n"
    $bytes=[Text.Encoding]::UTF8.GetBytes($text)
    [IO.File]::WriteAllBytes($file,$bytes)
    $result=& $script -Action clean -HostsPath $file -BackupDir $dir
    $clean=[IO.File]::ReadAllText($file)
    if ($clean -ne "# unchanged`r`n127.0.0.1 local.test`r`n1.1.1.1`tlocal.alias # mixed`r`n8.8.8.8 github.com.evil.test`r`n") {throw 'Unexpected cleanup result'}
    & $script -Action restore -HostsPath $file -Backup $result.backup
    if ([Convert]::ToBase64String([IO.File]::ReadAllBytes($file)) -ne [Convert]::ToBase64String($bytes)) {throw 'Restore mismatch'}
    $result=& $script -Action clean -HostsPath $file -BackupDir $dir
    [IO.File]::AppendAllText($file,'# later edit')
    $refused=$false
    try { & $script -Action restore -HostsPath $file -Backup $result.backup } catch { $refused=$true }
    if (!$refused) {throw 'Concurrent edit was overwritten'}
    Write-Output 'PASS: unrelated entries, mixed aliases, CRLF, exact restoration and later-edit guard'
} finally {
    # This directory is the freshly generated test directory, not a computed user path.
    if ([IO.Path]::GetFullPath($dir).StartsWith([IO.Path]::GetFullPath([IO.Path]::GetTempPath())) -and (Split-Path $dir -Leaf) -like 'turbo-hosts-*') {Remove-Item -LiteralPath $dir -Recurse -Force}
}
