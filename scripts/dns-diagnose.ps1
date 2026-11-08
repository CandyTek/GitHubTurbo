param([string]$Proxy = 'http://127.0.0.1:10808')
$ErrorActionPreference = 'Stop'
$names = @('github.com', 'api.github.com', 'raw.githubusercontent.com', 'codeload.github.com', 'release-assets.githubusercontent.com')
$rows = foreach ($name in $names) {
    $system = @()
    $systemError = $null
    try { $system = @([Net.Dns]::GetHostAddresses($name) | ForEach-Object { $_.IPAddressToString }) } catch { $systemError = $_.Exception.Message }
    $answers = @()
    $dohError = $null
    try {
        $url = 'https://cloudflare-dns.com/dns-query?name=' + [uri]::EscapeDataString($name) + '&type=A'
        $reply = Invoke-RestMethod -Uri $url -Proxy $Proxy -Headers @{Accept='application/dns-json'} -TimeoutSec 10
        if ($reply.Status -ne 0) { throw "DNS status $($reply.Status)" }
        $answers = @($reply.Answer | Where-Object { $_.type -eq 1 } | Select-Object data,TTL)
    } catch { $dohError = $_.Exception.Message }
    [pscustomobject]@{host=$name;system=$system;system_error=$systemError;doh_ipv4=$answers;doh_error=$dohError}
}
$root = Split-Path $PSScriptRoot -Parent
New-Item -ItemType Directory -Force -Path (Join-Path $root 'reports') | Out-Null
$json = $rows | ConvertTo-Json -Depth 6
[IO.File]::WriteAllText((Join-Path $root 'reports\dns.json'), $json, [Text.UTF8Encoding]::new($false))
$json
