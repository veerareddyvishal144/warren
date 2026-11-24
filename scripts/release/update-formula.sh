#!/usr/bin/env bash
# Point the Homebrew formula at a release: rewrite its version, download URLs
# and sha256 digests from that release's SHA256SUMS.
#
# Usage:
#   scripts/release/update-formula.sh --version X.Y.Z --sums SHA256SUMS
#                                     [--formula packaging/homebrew/warren.rb]
#                                     [--repo owner/name] [--allow-missing]
#
# The formula has one `url "...warren-<version>-<os>-<arch>.tar.gz"` line per
# platform, each followed by its `sha256 "..."` line. For every url line the
# script substitutes the new version into the URL and replaces the following
# sha256 with the digest SHA256SUMS records for that tarball; the `version
# "..."` line is updated too when the formula has one. A platform missing from SHA256SUMS is an error
# unless --allow-missing is given (its sha256 is then left as it was).
#
# Typical flow after the release workflow has run:
#   gh release download vX.Y.Z -p SHA256SUMS -D /tmp/warren-rel
#   scripts/release/update-formula.sh --version X.Y.Z --sums /tmp/warren-rel/SHA256SUMS
#   cp packaging/homebrew/warren.rb ../homebrew-warren/Formula/warren.rb   # the tap checkout
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
log() { printf '[update-formula] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

VERSION=""
SUMS=""
FORMULA="$ROOT/packaging/homebrew/warren.rb"
REPO="veerareddyvishal144/warren"
ALLOW_MISSING=0
while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="${2#v}"; shift 2 ;;
        --sums) SUMS="$2"; shift 2 ;;
        --formula) FORMULA="$2"; shift 2 ;;
        --repo) REPO="$2"; shift 2 ;;
        --allow-missing) ALLOW_MISSING=1; shift ;;
        -h|--help) sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1" ;;
    esac
done
[ -n "$VERSION" ] || die "--version is required"
[ -n "$SUMS" ] && [ -f "$SUMS" ] || die "--sums must name an existing SHA256SUMS file"
[ -f "$FORMULA" ] || die "$FORMULA does not exist"
grep -Eq '^[0-9a-f]{64}  warren-[^ ]+\.tar\.gz$' "$SUMS" || die "$SUMS has no 'digest  warren-*.tar.gz' lines"

BASE="https://github.com/$REPO/releases/download/v$VERSION"
tmp="$(mktemp "${TMPDIR:-/tmp}/warren-formula.XXXXXX")"
trap 'rm -f "$tmp"' EXIT

# awk is portable here (BWK awk on macOS, gawk or mawk on Linux): no gensub,
# no arrays of arrays. The platform (os-arch) is read out of the tarball name
# on the url line and remembered until the next sha256 line.
awk -v version="$VERSION" -v base="$BASE" -v sums="$SUMS" -v allow_missing="$ALLOW_MISSING" '
    BEGIN {
        while ((getline line < sums) > 0) {
            n = split(line, f, "  ")
            if (n == 2 && match(f[2], /^warren-.*-(darwin|linux)-(arm64|x86_64)\.tar\.gz$/)) {
                plat = f[2]
                sub(/^warren-[^-]+-/, "", plat)
                sub(/\.tar\.gz$/, "", plat)
                digest[plat] = f[1]
            }
        }
        close(sums)
        pending = ""
        missing = 0
        replaced = 0
    }
    /^[[:space:]]*version[[:space:]]+"[^"]*"/ {
        sub(/"[^"]*"/, "\"" version "\"")
        print
        next
    }
    /^[[:space:]]*url[[:space:]]+"[^"]*warren-[^"]*-(darwin|linux)-(arm64|x86_64)\.tar\.gz"/ {
        line = $0
        match(line, /warren-[^"]*-(darwin|linux)-(arm64|x86_64)\.tar\.gz/)
        name = substr(line, RSTART, RLENGTH)
        plat = name
        sub(/^warren-[^-]+-/, "", plat)
        sub(/\.tar\.gz$/, "", plat)
        pending = plat
        match(line, /^[[:space:]]*url[[:space:]]+/)
        indent = substr(line, 1, RLENGTH)
        print indent "\"" base "/warren-" version "-" plat ".tar.gz\""
        next
    }
    pending != "" && /^[[:space:]]*sha256[[:space:]]+"[^"]*"/ {
        if (pending in digest) {
            sub(/"[^"]*"/, "\"" digest[pending] "\"")
            replaced++
            printf "[update-formula] %s: %s\n", pending, digest[pending] > "/dev/stderr"
        } else {
            printf "[update-formula] %s: not in SHA256SUMS\n", pending > "/dev/stderr"
            missing++
        }
        pending = ""
        print
        next
    }
    { print }
    END {
        if (replaced == 0) { print "[update-formula] error: no url/sha256 pairs found in the formula" > "/dev/stderr"; exit 1 }
        if (missing > 0 && allow_missing != 1) { print "[update-formula] error: SHA256SUMS lacks a tarball the formula needs (pass --allow-missing to keep the old digest)" > "/dev/stderr"; exit 1 }
    }
' "$FORMULA" > "$tmp"

mv -f "$tmp" "$FORMULA"
trap - EXIT
log "updated $FORMULA to $VERSION"
