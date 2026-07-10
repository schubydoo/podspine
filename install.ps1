#Requires -Version 5.1
<#
.SYNOPSIS
  Podspine installer (Windows, PowerShell).

    irm https://raw.githubusercontent.com/schubydoo/podspine/main/install.ps1 | iex

  Downloads the signed standalone podspine.exe from the latest GitHub release,
  verifies its SHA-256 against the release's checksums.txt, installs it under
  %LOCALAPPDATA%\Programs\podspine, and adds that directory to the user PATH.

  Podspine shells out to ffmpeg/ffprobe at runtime but does not vendor them -
  install them separately and keep them on PATH (this script warns if missing).

  Environment overrides:
    PODSPINE_VERSION       pin a specific release tag (e.g. X.Y.Z); default: latest release
    PODSPINE_INSTALL_DIR   install directory; default: %LOCALAPPDATA%\Programs\podspine
#>

$ErrorActionPreference = 'Stop'
# PowerShell 5.1 on older Windows can default to TLS 1.0, which GitHub rejects.
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Owner = 'schubydoo'
$Repo  = 'podspine'
$Tool  = 'podspine'

function Write-Info($m) { Write-Host "[INFO]  $m" -ForegroundColor Blue }
function Write-Ok($m)   { Write-Host "[ OK ]  $m" -ForegroundColor Green }
function Write-Warn($m) { Write-Host "[WARN]  $m" -ForegroundColor Yellow }
function Write-Err($m)  { Write-Host "[ERR ]  $m" -ForegroundColor Red }

function Show-Fallback {
    Write-Host ''
    Write-Host 'No standalone binary was installed. Try one of these instead:'
    Write-Host "  scoop bucket add $Repo https://github.com/$Owner/scoop-$Repo; scoop install $Tool"
    Write-Host "  docker run -v C:\books:/library -p 8080:8080 ghcr.io/$Owner/$Repo:latest"
    Write-Host "  cargo binstall --git https://github.com/$Owner/$Repo $Tool"
    Write-Host "Full install guide: https://$Owner.github.io/$Repo/latest/installation/"
}

function Test-Ffmpeg {
    $missing = @()
    if (-not (Get-Command ffmpeg  -ErrorAction SilentlyContinue)) { $missing += 'ffmpeg' }
    if (-not (Get-Command ffprobe -ErrorAction SilentlyContinue)) { $missing += 'ffprobe' }
    if ($missing.Count -gt 0) {
        Write-Warn ("{0} not found on PATH - Podspine needs them at runtime." -f ($missing -join ' and '))
        Write-Warn '  winget install Gyan.FFmpeg    (or: scoop install ffmpeg)'
    }
}

function Install-Podspine {
    Write-Info "Installing $Tool"

    # Windows publishes only the amd64 binary; Windows-on-ARM runs it under x64
    # emulation, so this is the right target on both AMD64 and ARM64.
    $target = 'windows-amd64'

    $ver = $env:PODSPINE_VERSION
    if ([string]::IsNullOrWhiteSpace($ver)) {
        Write-Info 'Resolving latest release...'
        $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$Owner/$Repo/releases/latest" `
            -Headers @{ 'User-Agent' = 'podspine-install'; 'Accept' = 'application/vnd.github+json' }
        $ver = $rel.tag_name -replace '^v', ''
    }
    if ([string]::IsNullOrWhiteSpace($ver)) { throw 'Could not resolve the latest release version.' }
    Write-Info "Arch: $env:PROCESSOR_ARCHITECTURE | Version: $ver"

    $asset = "$Tool-v$ver-$target.exe"
    $base  = "https://github.com/$Owner/$Repo/releases/download/v$ver"
    $work  = Join-Path ([System.IO.Path]::GetTempPath()) ($Tool + '-' + [System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Path $work -Force | Out-Null

    try {
        # checksums.txt is the authoritative list of published binaries - gate on it so
        # an arch with no binary falls back cleanly instead of 404-ing on download.
        $sumsPath = Join-Path $work 'checksums.txt'
        Invoke-WebRequest -Uri "$base/checksums.txt" -OutFile $sumsPath -UseBasicParsing
        # Exact field-2 (filename) match, mirroring install.sh's awk. sha256sum text
        # mode writes "<hash>  <name>"; TrimStart('*') tolerates a binary-mode marker.
        $expected = ''
        foreach ($l in Get-Content -LiteralPath $sumsPath) {
            $parts = $l -split '\s+', 2
            $name = if ($parts.Count -eq 2) { $parts[1].Trim().TrimStart('*') } else { '' }
            if ($parts.Count -eq 2 -and $name -eq $asset) {
                $expected = $parts[0].ToLower()
                break
            }
        }
        if ([string]::IsNullOrEmpty($expected)) {
            Write-Err "Release v$ver has no $target binary ($asset not in checksums.txt)."
            Show-Fallback
            # Signal failure to callers/CI without `exit` (which would kill an iex host).
            $global:LASTEXITCODE = 1
            return
        }

        Write-Info "Downloading $asset..."
        $exePath = Join-Path $work $asset
        Invoke-WebRequest -Uri "$base/$asset" -OutFile $exePath -UseBasicParsing

        Write-Info 'Verifying checksum...'
        $actual = (Get-FileHash -Algorithm SHA256 -Path $exePath).Hash.ToLower()
        if ($actual -ne $expected) {
            throw "Checksum mismatch for ${asset}: expected $expected, got $actual."
        }
        Write-Ok ('Checksum verified (sha256: {0}...)' -f $actual.Substring(0, 12))

        $dir = $env:PODSPINE_INSTALL_DIR
        if ([string]::IsNullOrWhiteSpace($dir)) {
            $dir = Join-Path $env:LOCALAPPDATA "Programs\$Tool"
        }
        New-Item -ItemType Directory -Path $dir -Force | Out-Null
        $dest = Join-Path $dir "$Tool.exe"
        Move-Item -Path $exePath -Destination $dest -Force
        # The download carries a mark-of-the-web zone tag; clear it now that the
        # checksum is verified so the user isn't blocked on first run.
        Unblock-File -Path $dest -ErrorAction SilentlyContinue
        Write-Ok "Installed $dest"

        # Add the install dir to the user PATH if it isn't already there.
        $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
        if ($null -eq $userPath) { $userPath = '' }
        if (($userPath -split ';') -notcontains $dir) {
            $newPath = if ([string]::IsNullOrEmpty($userPath)) { $dir } else { "$userPath;$dir" }
            [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
            Write-Warn "Added $dir to your user PATH - open a new terminal for it to take effect."
        }

        # Verify: require a zero exit AND a 'podspine' identity banner. A launch
        # failure (bad image, missing DLL) degrades to a warning, not a hard error.
        $banner = ''
        $confirmed = $false
        try {
            $banner = & $dest --version 2>$null
            if ($LASTEXITCODE -eq 0 -and "$banner" -match '^podspine') { $confirmed = $true }
        }
        catch { }
        if ($confirmed) {
            Write-Ok "$banner installed"
        }
        else {
            Write-Warn "Installed to $dest, but '$dest --version' did not confirm a podspine binary."
        }

        Test-Ffmpeg
        Write-Info "Run it with:  $Tool --library C:\path\to\audiobooks"
        Write-Info 'The binary is Sigstore-signed but not authenticode-signed, so SmartScreen may warn on first run.'
        Write-Ok 'Installation complete!'
        # Only reached on success. The `--version` probe above can leave $LASTEXITCODE
        # non-zero without throwing; normalize so a successful install reports 0 to a
        # piped `iex` caller / CI. (Failure paths return/throw before here with 1.)
        $global:LASTEXITCODE = 0
    }
    finally {
        Remove-Item -Path $work -Recurse -Force -ErrorAction SilentlyContinue
    }
}

# Run via a function + try/catch (no `exit`, which would terminate the host shell
# when this script is piped into `iex`).
try {
    Install-Podspine
}
catch {
    Write-Err $_.Exception.Message
    # Signal failure to callers/CI without `exit` (which would kill an iex host).
    $global:LASTEXITCODE = 1
}
