#!/bin/sh
# Install Warren (warrend, warren, warren-mcp, warren-gateway) from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/veerareddyvishal144/warren/main/scripts/install.sh | sh
#
# Options, as environment variables or flags (flags win):
#   WARREN_VERSION      --version X.Y.Z     release to install (default: the latest release)
#   WARREN_INSTALL_DIR  --install-dir DIR   where the binaries go (default: /usr/local/bin when
#                                           writable, else ~/.local/bin; no sudo is ever run)
#   WARREN_REPO                             GitHub repository (default: veerareddyvishal144/warren)
#   WARREN_BASE_URL                         where the tarball and SHA256SUMS are fetched from
#                                           (default: the release's download URL on GitHub; a
#                                           mirror or an internal server works the same way)
#
#   curl -fsSL .../install.sh | WARREN_VERSION=0.1.0 sh
#   curl -fsSL .../install.sh | sh -s -- --install-dir ~/bin
#   curl -fsSL .../install.sh | sudo sh              # system-wide into /usr/local/bin
#
# What it does: picks the tarball for this OS and CPU (darwin-arm64,
# linux-x86_64, linux-arm64), downloads it with the release's SHA256SUMS,
# verifies the digest, unpacks into a temporary directory and installs the four
# binaries with `install -m 0755`. Nothing else is written. Portable POSIX sh:
# runs under dash, bash, zsh and BusyBox ash.
set -eu

REPO="${WARREN_REPO:-veerareddyvishal144/warren}"
VERSION="${WARREN_VERSION:-}"
INSTALL_DIR="${WARREN_INSTALL_DIR:-}"
# Oldest glibc the Linux tarballs run on; the release workflow builds them in
# an ubuntu:22.04 container (see .github/workflows/release.yml).
GLIBC_MIN_MAJOR=2
GLIBC_MIN_MINOR=35

log() { printf '[warren-install] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

usage() {
    sed -n '2,/^set -eu/p' "$0" 2>/dev/null | sed '$d' | sed 's/^# \{0,1\}//' || true
}

while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a value"; VERSION="$2"; shift 2 ;;
        --version=*) VERSION="${1#--version=}"; shift ;;
        --install-dir) [ $# -ge 2 ] || die "--install-dir needs a value"; INSTALL_DIR="$2"; shift 2 ;;
        --install-dir=*) INSTALL_DIR="${1#--install-dir=}"; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option $1 (try --help)" ;;
    esac
done

for tool in curl tar install uname mktemp; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done
if command -v sha256sum >/dev/null 2>&1; then
    SHA256="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    SHA256="shasum -a 256"
else
    die "sha256sum or shasum is required to verify the download"
fi

# --- platform ------------------------------------------------------------------
os="$(uname -s)"
cpu="$(uname -m)"
case "$os" in
    Darwin) OS=darwin ;;
    Linux) OS=linux ;;
    *) die "unsupported operating system $os; Warren runs on macOS (Apple silicon) and Linux" ;;
esac
case "$cpu" in
    arm64|aarch64) ARCH=arm64 ;;
    x86_64|amd64) ARCH=x86_64 ;;
    *) die "unsupported CPU $cpu; releases cover arm64 and x86_64" ;;
esac
if [ "$OS" = darwin ] && [ "$ARCH" != arm64 ]; then
    die "no prebuilt binaries for Intel Macs (the mac instance type needs Apple silicon); build from source: https://github.com/$REPO/blob/main/docs/install.md#from-source"
fi
if [ "$OS" = linux ]; then
    # The daemon, CLI and MCP server link glibc; only warren-gateway is static.
    if command -v ldd >/dev/null 2>&1; then
        ldd_out="$(ldd --version 2>&1 | head -n1 || true)"
        case "$ldd_out" in
            *musl*) die "this Linux uses musl libc (Alpine?); the release binaries need glibc $GLIBC_MIN_MAJOR.$GLIBC_MIN_MINOR or newer. Build from source or use the Docker image: https://github.com/$REPO/blob/main/docs/install.md" ;;
        esac
        glibc="$(printf '%s' "$ldd_out" | sed -n 's/.*[^0-9]\([0-9][0-9]*\.[0-9][0-9]*\)$/\1/p')"
        if [ -n "$glibc" ]; then
            major="${glibc%%.*}"
            minor="${glibc#*.}"
            if [ "$major" -lt "$GLIBC_MIN_MAJOR" ] || { [ "$major" -eq "$GLIBC_MIN_MAJOR" ] && [ "$minor" -lt "$GLIBC_MIN_MINOR" ]; }; then
                die "glibc $glibc is older than $GLIBC_MIN_MAJOR.$GLIBC_MIN_MINOR, the floor of the release binaries (Debian 12, Ubuntu 22.04 and newer work). Build from source or use the Docker image: https://github.com/$REPO/blob/main/docs/install.md"
            fi
        fi
    fi
fi

# --- version -------------------------------------------------------------------
if [ -z "$VERSION" ]; then
    # /releases/latest redirects to /releases/tag/<tag>: no API quota, no JSON.
    final="$(curl -fsSL --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest" 2>/dev/null || true)"
    case "$final" in
        */releases/tag/*) VERSION="${final##*/releases/tag/}" ;;
    esac
    if [ -z "$VERSION" ]; then
        json="$(curl -fsSL --proto '=https' --tlsv1.2 -H 'Accept: application/vnd.github+json' "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null || true)"
        VERSION="$(printf '%s' "$json" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1)"
    fi
    [ -n "$VERSION" ] || die "could not determine the latest release of $REPO (no release yet, or no network); set WARREN_VERSION=X.Y.Z"
fi
VERSION="${VERSION#v}"
TAG="v$VERSION"
NAME="warren-$VERSION-$OS-$ARCH"
TARBALL="$NAME.tar.gz"
BASE_URL="${WARREN_BASE_URL:-https://github.com/$REPO/releases/download/$TAG}"
BASE_URL="${BASE_URL%/}"
# Only https when talking to GitHub; a mirror may be plain http on a LAN.
case "$BASE_URL" in
    https://*) CURL="curl -fsSL --proto =https --tlsv1.2 --retry 3 --retry-delay 2" ;;
    *) CURL="curl -fsSL --retry 3 --retry-delay 2" ;;
esac

# --- install directory ---------------------------------------------------------
if [ -n "$INSTALL_DIR" ]; then
    mkdir -p "$INSTALL_DIR" 2>/dev/null || die "cannot create $INSTALL_DIR"
    [ -w "$INSTALL_DIR" ] || die "$INSTALL_DIR is not writable (run with sudo, or choose another WARREN_INSTALL_DIR)"
else
    INSTALL_DIR=/usr/local/bin
    if ! { [ -d "$INSTALL_DIR" ] && [ -w "$INSTALL_DIR" ]; }; then
        INSTALL_DIR="$HOME/.local/bin"
        log "/usr/local/bin is not writable; installing into $INSTALL_DIR (set WARREN_INSTALL_DIR or run with sudo for a system-wide install)"
        mkdir -p "$INSTALL_DIR" || die "cannot create $INSTALL_DIR"
    fi
fi

# --- download and verify -------------------------------------------------------
TMP="$(mktemp -d "${TMPDIR:-/tmp}/warren-install.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT INT TERM

log "downloading $BASE_URL/$TARBALL"
$CURL -o "$TMP/$TARBALL" "$BASE_URL/$TARBALL" \
    || die "download failed; does release $TAG of $REPO exist and does it ship $TARBALL? https://github.com/$REPO/releases"
$CURL -o "$TMP/SHA256SUMS" "$BASE_URL/SHA256SUMS" \
    || die "could not download SHA256SUMS for $TAG; refusing to install an unverified tarball"

expected="$(grep "  $TARBALL\$" "$TMP/SHA256SUMS" | head -n1 | cut -d' ' -f1)"
[ -n "$expected" ] || die "SHA256SUMS of $TAG has no entry for $TARBALL"
actual="$(cd "$TMP" && $SHA256 "$TARBALL" | cut -d' ' -f1)"
[ "$actual" = "$expected" ] || die "checksum mismatch for $TARBALL: expected $expected, got $actual"
log "sha256 verified"

tar -xzf "$TMP/$TARBALL" -C "$TMP"
SRC="$TMP/$NAME"
for b in warrend warren warren-mcp warren-gateway; do
    [ -f "$SRC/$b" ] || die "$TARBALL does not contain $b"
done

# --- install -------------------------------------------------------------------
# Atomic replacement: a running warrend keeps its old inode; restart it to upgrade.
for b in warrend warren warren-mcp warren-gateway; do
    install -m 0755 "$SRC/$b" "$INSTALL_DIR/.$b.tmp.$$"
    mv -f "$INSTALL_DIR/.$b.tmp.$$" "$INSTALL_DIR/$b"
    if [ "$OS" = darwin ] && command -v xattr >/dev/null 2>&1; then
        xattr -d com.apple.quarantine "$INSTALL_DIR/$b" 2>/dev/null || true
    fi
done
installed="$("$INSTALL_DIR/warren" --version 2>/dev/null || true)"
[ -n "$installed" ] || die "$INSTALL_DIR/warren does not run on this machine"
log "installed $installed into $INSTALL_DIR (warrend, warren, warren-mcp, warren-gateway)"

on_path=0
case ":$PATH:" in
    *":$INSTALL_DIR:"*) on_path=1 ;;
esac

echo
echo "Warren $VERSION is installed in $INSTALL_DIR."
if [ "$on_path" = 0 ]; then
    echo
    echo "  $INSTALL_DIR is not on your PATH. Add it, for example:"
    echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi
echo
echo "Next steps:"
echo "  warren up                 start the daemon on this machine (Docker on Linux)"
if [ "$OS" = darwin ]; then
    echo "  warren mac init           prepare this Mac for macOS instances (Lume, base VM)"
fi
echo "  warren connect claude     add the Warren MCP server to Claude Code"
echo
echo "Docs: https://github.com/$REPO/blob/$TAG/docs/install.md"
