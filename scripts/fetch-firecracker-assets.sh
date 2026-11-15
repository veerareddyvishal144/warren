#!/usr/bin/env bash
# Download the Firecracker v1.18.0 release binary and the newest 6.1.x guest
# kernel published by the Firecracker CI into an assets directory, verifying
# sizes (and the release's SHA256 sums when curl can fetch them).
#
#   scripts/fetch-firecracker-assets.sh [DEST]      default DEST=/var/lib/warren/firecracker
#
# Produces DEST/firecracker and DEST/vmlinux (plus DEST/vmlinux-6.1.<patch>),
# which is what `warrend --firecracker-bin DEST/firecracker --kernel-image
# DEST/vmlinux` (or WARREN_FIRECRACKER_BIN / WARREN_KERNEL_IMAGE) expects.
#
# On this development host the same files already exist under
# /data/sandbox-probe/: `firecracker` (v1.18.0, 3921872 bytes), the release
# tarball `fc.tgz` with the `release-v1.18.0-x86_64/` directory next to it, and
# `vmlinux-6.1.155` (44279576 bytes). Point the daemon at those instead of
# downloading them again: WARREN_FIRECRACKER_BIN=/data/sandbox-probe/firecracker
# WARREN_KERNEL_IMAGE=/data/sandbox-probe/vmlinux-6.1.155.
set -euo pipefail

FC_VERSION="${FC_VERSION:-v1.18.0}"
ARCH="$(uname -m)"                      # x86_64 or aarch64
DEST="${1:-/var/lib/warren/firecracker}"
RELEASE_BASE="https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}"
TARBALL="firecracker-${FC_VERSION}-${ARCH}.tgz"
CI_BUCKET="https://s3.amazonaws.com/spec.ccfc.min"
CI_PREFIX="firecracker-ci/v1.15/${ARCH}"
KERNEL_SERIES="${KERNEL_SERIES:-6.1}"
MIN_FC_BYTES=1000000                    # the static binary is a few MB
MIN_KERNEL_BYTES=10000000               # an uncompressed vmlinux is tens of MB

log() { printf '%s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

command -v curl >/dev/null || die "curl is required"
command -v tar  >/dev/null || die "tar is required"

mkdir -p "$DEST"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/warren-fc-assets.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# Size reported by the server for a URL (empty when unknown).
remote_size() {
  curl -fsSLI "$1" | tr -d '\r' | awk 'tolower($1)=="content-length:" {n=$2} END {print n}'
}

# Download URL to FILE, then verify the byte count against Content-Length when
# the server sent one and against a minimum otherwise.
fetch() {
  local url="$1" file="$2" min="$3" expected actual
  log "downloading $url"
  curl -fSL --retry 3 --retry-delay 2 -o "$file" "$url"
  actual="$(stat -c %s "$file")"
  expected="$(remote_size "$url" || true)"
  if [ -n "$expected" ] && [ "$expected" != "$actual" ]; then
    die "$file is $actual bytes, server announced $expected"
  fi
  [ "$actual" -ge "$min" ] || die "$file is only $actual bytes; download looks truncated"
}

# --- Firecracker release ----------------------------------------------------
fetch "${RELEASE_BASE}/${TARBALL}" "$WORK/$TARBALL" "$MIN_FC_BYTES"
if curl -fsSL -o "$WORK/${TARBALL}.sha256.txt" "${RELEASE_BASE}/${TARBALL}.sha256.txt" 2>/dev/null; then
  (cd "$WORK" && sha256sum -c --quiet "${TARBALL}.sha256.txt") || die "SHA256 mismatch for $TARBALL"
  log "sha256 verified for $TARBALL"
else
  log "no sha256 file published for $TARBALL; size check only"
fi
tar -xzf "$WORK/$TARBALL" -C "$WORK"
FC_BIN="$(find "$WORK" -type f -name "firecracker-${FC_VERSION}-${ARCH}" | head -n1)"
[ -n "$FC_BIN" ] || die "firecracker binary not found inside $TARBALL"
# The release also ships SHA256SUMS for the unpacked files, as
# "<sha256>  ./firecracker-v1.18.0-x86_64" lines relative to the release directory.
if [ -f "$(dirname "$FC_BIN")/SHA256SUMS" ]; then
  (cd "$(dirname "$FC_BIN")" && grep -E "[ /]firecracker-${FC_VERSION}-${ARCH}\$" SHA256SUMS | sha256sum -c --quiet) \
    || die "SHA256SUMS mismatch for the firecracker binary"
  log "SHA256SUMS verified for the firecracker binary"
fi
install -m 0755 "$FC_BIN" "$DEST/firecracker"
"$DEST/firecracker" --version | head -n1 >&2

# --- Guest kernel ------------------------------------------------------------
# List the bucket and pick the highest 6.1.x patch level.
LISTING="$(curl -fsSL "${CI_BUCKET}/?prefix=${CI_PREFIX}/vmlinux-${KERNEL_SERIES}")"
KERNEL_KEY="$(printf '%s' "$LISTING" | grep -o "<Key>[^<]*</Key>" | sed 's/<[^>]*>//g' \
  | grep -E "/vmlinux-${KERNEL_SERIES//./\\.}\.[0-9]+$" | sort -t. -k3,3n | tail -n1)"
[ -n "$KERNEL_KEY" ] || die "no vmlinux-${KERNEL_SERIES}.x found under ${CI_BUCKET}/${CI_PREFIX}/"
KERNEL_NAME="$(basename "$KERNEL_KEY")"
# <Size> that follows this key in the XML.
KERNEL_SIZE="$(printf '%s' "$LISTING" | tr '<' '\n' | awk -v k="Key>${KERNEL_KEY}" '
  index($0, k)==1 {want=1} want && /^Size>/ {sub(/^Size>/, ""); print; exit}')"
fetch "${CI_BUCKET}/${KERNEL_KEY}" "$WORK/$KERNEL_NAME" "$MIN_KERNEL_BYTES"
if [ -n "$KERNEL_SIZE" ] && [ "$(stat -c %s "$WORK/$KERNEL_NAME")" != "$KERNEL_SIZE" ]; then
  die "$KERNEL_NAME size does not match the bucket listing ($KERNEL_SIZE)"
fi
install -m 0644 "$WORK/$KERNEL_NAME" "$DEST/$KERNEL_NAME"
ln -sfn "$KERNEL_NAME" "$DEST/vmlinux"

log "installed:"
ls -l "$DEST/firecracker" "$DEST/$KERNEL_NAME" "$DEST/vmlinux" >&2
