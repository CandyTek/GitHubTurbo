param(
    [Parameter(Mandatory = $true)][uri]$Url,
    [Parameter(Mandatory = $true)][string]$Output,
    [string]$Aria2,
    [string]$Proxy = ''
)
$ErrorActionPreference = 'Stop'
if ($Url.Scheme -ne 'https') { throw 'An HTTPS URL is required' }
if ($Aria2) {
    $full = [IO.Path]::GetFullPath($Output)
    $extra = @()
    if ($Proxy) { $extra += "--all-proxy=$Proxy" }
    $root = Split-Path $PSScriptRoot -Parent
    foreach ($candidate in @((Join-Path $root 'state\ca.pem'),(Join-Path $root 'dist\state\ca.pem'))) {
        if ((Test-Path -LiteralPath $candidate) -and (Test-Path -LiteralPath (Join-Path (Split-Path $candidate -Parent) 'session.json'))) { $extra += "--ca-certificate=$candidate"; break }
    }
    & $Aria2 @extra --split=4 --max-connection-per-server=4 --min-split-size=4M --continue=true --file-allocation=none --max-tries=3 --retry-wait=2 --check-certificate=true --auto-file-renaming=false "--dir=$([IO.Path]::GetDirectoryName($full))" "--out=$([IO.Path]::GetFileName($full))" $Url.AbsoluteUri
} else {
    $extra = if ($Proxy) { @('--proxy',$Proxy) } else { @('--noproxy','*') }
    & curl.exe @extra --fail --location --proto '=https' --proto-redir '=https' --connect-timeout 15 --retry 2 --continue-at - --output $Output $Url.AbsoluteUri
}
if ($LASTEXITCODE -ne 0) { throw "Download failed (exit $LASTEXITCODE)" }
