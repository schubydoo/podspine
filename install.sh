#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# Podspine installer
#
#   curl -fsSL https://raw.githubusercontent.com/schubydoo/podspine/main/install.sh | bash
#
# Downloads the signed standalone `podspine` binary for your OS + architecture
# from the latest GitHub release, verifies its SHA-256 against the release's
# signed checksums.txt, and installs it onto your PATH.
#
# Podspine shells out to `ffmpeg`/`ffprobe` at runtime but does not vendor them —
# install them separately and keep them on PATH (this script warns if they're
# missing).
#
# Environment overrides:
#   PODSPINE_VERSION       pin a specific release tag (e.g. X.Y.Z); default: latest release
#   PODSPINE_INSTALL_DIR   install directory; default: ~/.local/bin
# ============================================================================

REPO_OWNER="schubydoo"
REPO_NAME="podspine"
TOOL_NAME="podspine"

# Scratch dir, cleaned up on exit. Global so the EXIT trap can see it even after
# main() returns (a `local` would be out of scope and trip `set -u`).
WORKDIR=""
cleanup() { [ -n "$WORKDIR" ] && rm -rf "$WORKDIR"; }
trap cleanup EXIT

# --- Colour output (disabled when stdout is not a TTY) ---
if [ -t 1 ]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BLUE='\033[0;34m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BLUE=''; NC=''
fi
info() { printf "${BLUE}[INFO]${NC}  %s\n" "$*"; }
ok()   { printf "${GREEN}[ OK ]${NC}  %s\n" "$*"; }
warn() { printf "${YELLOW}[WARN]${NC}  %s\n" "$*"; }
err()  { printf "${RED}[ERR ]${NC}  %s\n" "$*" >&2; }
die()  { err "$@"; exit 1; }

# --- Fallback hint for unsupported targets ---
fallback() {
    cat >&2 <<EOF

No standalone binary is published for this OS/architecture (yet). Alternatives:

  docker run -v /books:/library -p 8080:8080 ghcr.io/${REPO_OWNER}/${REPO_NAME}:latest
  cargo binstall --git https://github.com/${REPO_OWNER}/${REPO_NAME} ${TOOL_NAME}
  brew install ${REPO_OWNER}/${REPO_NAME}/${TOOL_NAME}   # macOS / Linux

Full install guide: https://${REPO_OWNER}.github.io/${REPO_NAME}/latest/installation/
EOF
}

# --- Detection ---
detect_os() {
    local os; os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    case "$os" in
        linux*)               echo "linux" ;;
        darwin*)              echo "darwin" ;;
        mingw*|msys*|cygwin*) echo "windows" ;;
        *)                    die "Unsupported operating system: $os" ;;
    esac
}

detect_arch() {
    local arch; arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64)  echo "amd64" ;;
        aarch64|arm64) echo "arm64" ;;
        *)             echo "$arch" ;;  # surfaced verbatim in the unsupported-target error
    esac
}

# Map (os, arch, ver) -> release asset basename, or empty for an unknown arch.
# Whether the named asset actually exists in a given release is decided later,
# against that release's checksums.txt (the authoritative list of built binaries).
asset_for() {
    local os="$1" arch="$2" ver="$3"
    case "${os}-${arch}" in
        linux-amd64|linux-arm64|darwin-amd64|darwin-arm64)
            echo "${TOOL_NAME}-v${ver}-${os}-${arch}" ;;
        *)  echo "" ;;
    esac
}

# --- HTTP helpers (curl or wget) ---
have() { command -v "$1" >/dev/null 2>&1; }

http_to() {  # http_to <url> <dest>
    local url="$1" dest="$2"
    if have curl; then
        curl -fsSL -o "$dest" "$url"
    elif have wget; then
        wget -qO "$dest" "$url"
    else
        die "Need curl or wget to download files."
    fi
}

resolve_latest() {  # echo the latest version (tag minus leading v)
    local url tag
    if have curl; then
        url="$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
            "https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/latest")"
    elif have wget; then
        # HTTP headers are CRLF-terminated, so strip the trailing CR — otherwise the
        # version carries a \r into the asset URL and 404s on wget-only hosts.
        url="$(wget -q -S -O /dev/null \
            "https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/latest" 2>&1 \
            | awk '/^[[:space:]]*Location:/ {print $2}' | tail -n1 | tr -d '\r')"
    else
        die "Need curl or wget to resolve the latest version."
    fi
    tag="${url##*/tag/}"          # .../releases/tag/v1.1.0 -> v1.1.0
    tag="${tag#v}"               # v1.1.0 -> 1.1.0
    [ -n "$tag" ] && [ "$tag" != "$url" ] || die "Could not resolve the latest release version."
    echo "$tag"
}

sha256_of() {  # echo the sha256 of a file, portably
    if have sha256sum; then
        sha256sum "$1" | awk '{print $1}'
    elif have shasum; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        die "Need sha256sum or shasum to verify the download."
    fi
}

# --- Install-dir selection ---
choose_install_dir() {
    if [ -n "${PODSPINE_INSTALL_DIR:-}" ]; then
        echo "$PODSPINE_INSTALL_DIR"; return
    fi
    echo "${HOME}/.local/bin"
}

# --- Runtime dependency check (warn only) ---
check_ffmpeg() {
    local missing=""
    have ffmpeg  || missing="ffmpeg"
    have ffprobe || missing="${missing:+$missing and }ffprobe"
    if [ -n "$missing" ]; then
        warn "${missing} not found on PATH — Podspine needs them at runtime to probe"
        warn "and split audiobooks. Install ffmpeg (it provides ffprobe), e.g.:"
        warn "  apt install ffmpeg   |   dnf install ffmpeg   |   brew install ffmpeg"
    fi
}

main() {
    info "Installing ${TOOL_NAME}"

    local os arch ver asset
    os="$(detect_os)"
    arch="$(detect_arch)"

    if [ "$os" = "windows" ]; then
        warn "On Windows, install with the PowerShell script or Scoop instead:"
        warn "  irm https://raw.githubusercontent.com/${REPO_OWNER}/${REPO_NAME}/main/install.ps1 | iex"
        warn "  scoop bucket add ${REPO_NAME} https://github.com/${REPO_OWNER}/scoop-${REPO_NAME}; scoop install ${TOOL_NAME}"
        exit 1
    fi

    ver="${PODSPINE_VERSION:-}"
    if [ -z "$ver" ]; then
        info "Resolving latest release..."
        ver="$(resolve_latest)"
    fi
    info "OS: ${os} | Arch: ${arch} | Version: ${ver}"

    asset="$(asset_for "$os" "$arch" "$ver")"
    if [ -z "$asset" ]; then
        err "No standalone binary for ${os}-${arch}."
        fallback
        exit 1
    fi

    local base="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download/v${ver}"
    WORKDIR="$(mktemp -d)" || die "Could not create a temporary directory."

    # The release's checksums.txt is the authoritative list of published binaries.
    # If our target isn't listed, no binary was built for this arch in this
    # release — fail cleanly with alternatives rather than 404 on download.
    http_to "${base}/checksums.txt" "${WORKDIR}/checksums.txt"
    local expected actual
    # Exact field-2 (filename) match; TrimStart('*') tolerates a binary-mode marker
    # ("<hash> *<name>"). Our releases use text mode ("<hash>  <name>").
    expected="$(awk -v a="$asset" '{n=$2; sub(/^\*/,"",n); if (n==a) print $1}' "${WORKDIR}/checksums.txt")"
    if [ -z "$expected" ]; then
        err "Release v${ver} has no ${os}-${arch} binary (${asset} not in checksums.txt)."
        fallback
        exit 1
    fi

    info "Downloading ${asset}..."
    http_to "${base}/${asset}" "${WORKDIR}/${asset}"

    info "Verifying checksum..."
    actual="$(sha256_of "${WORKDIR}/${asset}")"
    if [ "$expected" != "$actual" ]; then
        die "Checksum mismatch for ${asset}: expected ${expected}, got ${actual}."
    fi
    ok "Checksum verified (sha256: ${actual:0:12}...)"

    local dir; dir="$(choose_install_dir)"
    mkdir -p "$dir" || die "Cannot create install directory: $dir"
    local dest="${dir}/${TOOL_NAME}"

    # Write atomically; use sudo only if the directory is not writable.
    chmod +x "${WORKDIR}/${asset}"
    if [ -w "$dir" ]; then
        mv -f "${WORKDIR}/${asset}" "$dest"
    elif have sudo; then
        warn "${dir} is not writable — using sudo to install."
        sudo mv -f "${WORKDIR}/${asset}" "$dest"
    else
        die "Cannot write to ${dir}. Re-run with PODSPINE_INSTALL_DIR=~/.local/bin, or install sudo."
    fi
    ok "Installed ${dest}"

    # PATH hint
    case ":${PATH}:" in
        *":${dir}:"*) : ;;
        *) warn "${dir} is not on your PATH. Add it, e.g.:"
           warn "  echo 'export PATH=\"${dir}:\$PATH\"' >> ~/.bashrc && source ~/.bashrc" ;;
    esac

    # Verify: require a zero exit AND a 'podspine' identity banner — not merely
    # non-empty output (a failing binary could still print something to stdout).
    local got_ver=""
    if got_ver="$("$dest" --version 2>/dev/null)" && printf '%s' "$got_ver" | grep -qi '^podspine'; then
        ok "${got_ver} installed"
    else
        warn "Installed to ${dest}, but '${dest} --version' did not confirm a podspine binary."
    fi

    check_ffmpeg
    info "Run it with:  ${TOOL_NAME} --library /path/to/audiobooks"
    ok "Installation complete!"
}

main "$@"
