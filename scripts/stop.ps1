$root=Split-Path $PSScriptRoot -Parent
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $root 'dist\windows.ps1') -Action disable
exit $LASTEXITCODE
