#!/usr/bin/env bash
# Write SHA256SUMS for the release tarballs in a directory.
#
# Usage:
#   scripts/release/checksums.sh DIR          write DIR/SHA256SUMS from DIR/warren-*.tar.gz
#   scripts/release/checksums.sh --check DIR  verify DIR/SHA256SUMS against the files
#
# Every warren-*.tar.gz.sha256 that scripts/release/package.sh left next to a
# tarball is compared with a fresh digest first, so a tarball that changed
# between the build job and the release job fails the release. The output is
# one `sha256sum` line per tarball, sorted by name, which `sha256sum -c` and
# `shasum -a 256 -c` both read.
set -euo pipefail

log() { printf '[checksums] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "sha256sum or shasum is required"
    fi
}

CHECK=0
if [ "${1:-}" = "--check" ]; then
    CHECK=1
    shift
fi
DIR="${1:-}"
[ -n "$DIR" ] && [ -d "$DIR" ] || die "usage: $0 [--check] DIR"
cd "$DIR"

if [ "$CHECK" = 1 ]; then
    [ -f SHA256SUMS ] || die "no SHA256SUMS in $DIR"
    failed=0
    while read -r sum name; do
        [ -n "$name" ] || continue
        [ -f "$name" ] || { log "MISSING  $name"; failed=1; continue; }
        if [ "$(sha256_of "$name")" = "$sum" ]; then
            log "OK       $name"
        else
            log "FAILED   $name"
            failed=1
        fi
    done < SHA256SUMS
    [ "$failed" = 0 ] || die "checksum verification failed"
    exit 0
fi

found=0
: > SHA256SUMS.tmp
for f in warren-*.tar.gz; do
    [ -f "$f" ] || continue
    found=1
    sum="$(sha256_of "$f")"
    if [ -f "$f.sha256" ]; then
        recorded="$(cut -d' ' -f1 < "$f.sha256")"
        [ "$recorded" = "$sum" ] || die "$f does not match $f.sha256 (recorded $recorded, got $sum)"
    fi
    printf '%s  %s\n' "$sum" "$f" >> SHA256SUMS.tmp
done
[ "$found" = 1 ] || die "no warren-*.tar.gz in $DIR"
sort -k2 SHA256SUMS.tmp > SHA256SUMS
rm -f SHA256SUMS.tmp
log "wrote $DIR/SHA256SUMS:"
cat SHA256SUMS >&2
