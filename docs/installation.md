# Installing Podspine

Podspine ships a single static binary (plus a Docker image). It needs **`ffmpeg`
and `ffprobe` on your `PATH`** at runtime — every method below assumes they're
installed (the scripts and the Homebrew formula help with this).

Pick whichever fits your setup:

| Method | Best for | Command |
|---|---|---|
| [Docker](#docker) | Servers / homelab (recommended) | `docker run … ghcr.io/schubydoo/podspine` |
| [Install script](#linux-macos-script) | Linux / macOS, no package manager | `curl … \| bash` |
| [PowerShell](#windows-powershell) | Windows | `irm … \| iex` |
| [Homebrew](#homebrew) | macOS / Linux | `brew install schubydoo/podspine/podspine` |
| [Scoop](#scoop-windows) | Windows | `scoop install podspine` |
| [Nix](#nix) | NixOS / Nix users | `nix profile install github:schubydoo/podspine` |
| [Cargo](#cargo) | Rust users / from source | `cargo binstall --git … podspine` |

## Docker

The primary path for running Podspine as a server — see the
[Deploying](DEPLOYMENT.md) guide for volumes, compose, and reverse-proxy setup.
`ffmpeg` is bundled in the image.

```bash
docker run -v /path/to/audiobooks:/library:ro -v podspine-data:/data \
  -p 8080:8080 -e PODSPINE_BASE_URL=http://<your-lan-ip>:8080 \
  ghcr.io/schubydoo/podspine:latest
```

## Linux / macOS (script)

Downloads the signed binary for your OS/arch, verifies its SHA-256 against the
release `checksums.txt`, and installs to `~/.local/bin`:

```bash
curl -fsSL https://raw.githubusercontent.com/schubydoo/podspine/main/install.sh | bash
```

Overrides (environment variables):

| Var | Default | Purpose |
|---|---|---|
| `PODSPINE_VERSION` | latest release | Pin a version, e.g. `1.2.0` |
| `PODSPINE_INSTALL_DIR` | `~/.local/bin` | Install location (uses `sudo` only if it isn't writable) |

```bash
# Pin a version and install system-wide:
curl -fsSL https://raw.githubusercontent.com/schubydoo/podspine/main/install.sh \
  | PODSPINE_VERSION=1.2.0 PODSPINE_INSTALL_DIR=/usr/local/bin bash
```

**Uninstall** (removes the binary only — never your library or data dir):

```bash
curl -fsSL https://raw.githubusercontent.com/schubydoo/podspine/main/uninstall.sh | bash
```

## Windows (PowerShell)

Installs `podspine.exe` under `%LOCALAPPDATA%\Programs\podspine` and adds it to
your user `PATH`:

```powershell
irm https://raw.githubusercontent.com/schubydoo/podspine/main/install.ps1 | iex
```

Same overrides apply (`$env:PODSPINE_VERSION`, `$env:PODSPINE_INSTALL_DIR`).
Uninstall:

```powershell
irm https://raw.githubusercontent.com/schubydoo/podspine/main/uninstall.ps1 | iex
```

The binary is Sigstore-signed but not Authenticode-signed, so SmartScreen may
warn on first run — the installer clears the mark-of-the-web after verifying the
checksum.

## Homebrew

macOS and Linux. The formula pulls the release binary and declares `ffmpeg` as a
dependency:

```bash
brew install schubydoo/podspine/podspine
# or, to keep the tap around for upgrades:
brew tap schubydoo/podspine
brew install podspine
```

`brew upgrade podspine` tracks new releases.

## Scoop (Windows)

```powershell
scoop bucket add podspine https://github.com/schubydoo/scoop-podspine
scoop install podspine
```

## Nix

With flakes enabled:

```bash
# Run without installing:
nix run github:schubydoo/podspine -- --library ./books

# Install into your profile:
nix profile install github:schubydoo/podspine
```

On NixOS, the flake also exposes a module — add Podspine as a service:

```nix
{
  inputs.podspine.url = "github:schubydoo/podspine";
  # in your configuration:
  services.podspine = {
    enable = true;
    library = "/srv/audiobooks";
    baseUrl = "http://nas.lan:8080";
  };
}
```

## Cargo

Podspine is not published to crates.io (the workspace is `publish = false`), so
Cargo installs come from the Git repo.

With [`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall) — fetches the
prebuilt release binary, no compiling:

```bash
cargo binstall --git https://github.com/schubydoo/podspine podspine
```

Or build from source (`cargo install`):

```bash
cargo install --git https://github.com/schubydoo/podspine podspine
```

Or from a checkout:

```bash
git clone https://github.com/schubydoo/podspine && cd podspine
cargo build --release          # target/release/podspine
```

## Verifying a download

Every release ships a cosign-signed `checksums.txt` and SLSA provenance. To
verify a binary you downloaded manually:

```bash
cosign verify-blob --bundle checksums.txt.sigstore.json checksums.txt
sha256sum -c checksums.txt         # (from the dir holding the downloaded files)
gh attestation verify ./podspine-vX.Y.Z-linux-amd64 --owner schubydoo
```

The install scripts do the SHA-256 check for you; see
[SECURITY.md](https://github.com/schubydoo/podspine/blob/main/SECURITY.md#release-artifacts)
for the full verification story.

## Installing ffmpeg

Podspine needs `ffmpeg` (which provides `ffprobe`) at runtime:

| Platform | Command |
|---|---|
| Debian/Ubuntu | `apt install ffmpeg` |
| Fedora | `dnf install ffmpeg` |
| Arch | `pacman -S ffmpeg` |
| macOS | `brew install ffmpeg` |
| Windows | `winget install Gyan.FFmpeg` or `scoop install ffmpeg` |

Docker, Homebrew, and Nix installs pull it in for you.
