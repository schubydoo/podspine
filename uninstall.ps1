#Requires -Version 5.1
<#
.SYNOPSIS
  Podspine uninstaller (Windows, PowerShell).

    irm https://raw.githubusercontent.com/schubydoo/podspine/main/uninstall.ps1 | iex

  Removes the podspine.exe installed by install.ps1 and drops its directory from
  the user PATH. It does NOT touch your audiobook library or any --data-dir
  contents - delete those yourself if you want them gone.

  Environment overrides:
    PODSPINE_INSTALL_DIR   directory to remove from; default: %LOCALAPPDATA%\Programs\podspine
#>

$ErrorActionPreference = 'Stop'
$Tool = 'podspine'

function Write-Info($m) { Write-Host "[INFO]  $m" -ForegroundColor Blue }
function Write-Ok($m)   { Write-Host "[ OK ]  $m" -ForegroundColor Green }
function Write-Warn($m) { Write-Host "[WARN]  $m" -ForegroundColor Yellow }
function Write-Err($m)  { Write-Host "[ERR ]  $m" -ForegroundColor Red }

function Uninstall-Podspine {
    $dir = $env:PODSPINE_INSTALL_DIR
    if ([string]::IsNullOrWhiteSpace($dir)) {
        $dir = Join-Path $env:LOCALAPPDATA "Programs\$Tool"
    }
    $dest = Join-Path $dir "$Tool.exe"

    if (-not (Test-Path -LiteralPath $dest)) {
        Write-Warn "No $Tool.exe at $dest."
        $found = Get-Command $Tool -ErrorAction SilentlyContinue
        if ($found) {
            Write-Warn "A $Tool is on your PATH at $($found.Source) - if that's a Scoop/cargo"
            Write-Warn 'install, remove it with that tool instead (e.g. scoop uninstall podspine).'
        }
        $global:LASTEXITCODE = 0
        return
    }

    Write-Info "Removing $dest"
    Remove-Item -LiteralPath $dest -Force

    # Drop the install dir from the user PATH (only the entry we added).
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if ($userPath) {
        $kept = ($userPath -split ';') | Where-Object { $_ -and $_ -ne $dir }
        [Environment]::SetEnvironmentVariable('Path', ($kept -join ';'), 'User')
        Write-Info "Removed $dir from your user PATH - open a new terminal for it to take effect."
    }

    # Clean up the (now-empty) install directory if nothing else lives there.
    if ((Test-Path -LiteralPath $dir) -and -not (Get-ChildItem -LiteralPath $dir -Force)) {
        Remove-Item -LiteralPath $dir -Force
    }

    Write-Ok "Removed $Tool. Your audiobook library and any --data-dir contents were left untouched."
    $global:LASTEXITCODE = 0
}

try {
    Uninstall-Podspine
}
catch {
    Write-Err $_.Exception.Message
    $global:LASTEXITCODE = 1
}
