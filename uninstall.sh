#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# Podspine uninstaller
#
#   curl -fsSL https://raw.githubusercontent.com/schubydoo/podspine/main/uninstall.sh | bash
#
# Removes the `podspine` binary installed by install.sh. It does NOT touch your
# audiobook library or any data directory you passed to `--data-dir` — delete
# those yourself if you want them gone.
#
# Environment overrides:
#   PODSPINE_INSTALL_DIR   directory to remove from; default: ~/.local/bin
# ============================================================================

REPO_OWNER="schubydoo"   # kept for parity with install.sh; unused here
TOOL_NAME="podspine"

if [ -t 1 ]; then
    RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; BLUE='\033[0;34m'; NC='\033[0m'
else
    RED=''; GREEN=''; YELLOW=''; BLUE=''; NC=''
fi
info() { printf "${BLUE}[INFO]${NC}  %s\n" "$*"; }
ok()   { printf "${GREEN}[ OK ]${NC}  %s\n" "$*"; }
warn() { printf "${YELLOW}[WARN]${NC}  %s\n" "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

dir="${PODSPINE_INSTALL_DIR:-${HOME}/.local/bin}"
dest="${dir}/${TOOL_NAME}"

# If it isn't where we'd install it, try to find it on PATH so we can point the
# user at whatever they actually have (e.g. a brew/cargo copy we shouldn't touch).
if [ ! -e "$dest" ]; then
    warn "No ${TOOL_NAME} binary at ${dest}."
    if found="$(command -v "$TOOL_NAME" 2>/dev/null)"; then
        warn "A ${TOOL_NAME} is on your PATH at ${found} — if that's a package-manager"
        warn "install (brew, cargo, scoop, pacman, nix), remove it with that tool instead."
    fi
    exit 0
fi

info "Removing ${dest}"
if [ -w "$dir" ]; then
    rm -f "$dest"
elif have sudo; then
    warn "${dir} is not writable — using sudo to remove."
    sudo rm -f "$dest"
else
    printf "Cannot remove %s (not writable, no sudo). Remove it manually.\n" "$dest" >&2
    exit 1
fi
ok "Removed ${dest}"
info "Your audiobook library and any --data-dir contents were left untouched."
