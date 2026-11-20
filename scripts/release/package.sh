#!/usr/bin/env bash
# Build the Warren release tarball for the platform this script runs on.
#
# Usage:
#   scripts/release/package.sh [options]
#
# Options:
#   --version X.Y.Z   version the tarball claims; must equal the Cargo workspace
#                     version (default: the version in Cargo.toml)
#   --out DIR         where the tarball and its .sha256 go (default: $CARGO_TARGET_DIR/dist)
#   --no-build        package the binaries already in $CARGO_TARGET_DIR
#   --no-sign         macOS: skip the ad-hoc codesign step
#   --keep            keep the staging directory next to the tarball
#   -h, --help        this text
#
# Environment:
#   CARGO_TARGET_DIR    where cargo builds (default: target/ in the repository)
#   WARREN_MUSL_TARGET  Linux: the musl target of the gateway
#                       (default: <arch>-unknown-linux-musl)
#
# Produces, in --out:
#   warren-<version>-<os>-<arch>.tar.gz         one directory warren-<version>-<os>-<arch>/
#                                                with warrend, warren, warren-mcp,
#                                                warren-gateway and LICENSE
#   warren-<version>-<os>-<arch>.tar.gz.sha256  a `sha256sum` line; the release workflow
#                                                merges these into SHA256SUMS
#
# <os> is darwin or linux and <arch> is arm64 or x86_64, the names
# scripts/install.sh derives from uname. On Linux the gateway is a fully static
# musl binary because the daemon injects it into images (Debian, Alpine,
# distroless); the other three link the host's glibc. On macOS every binary is
# ad-hoc signed (codesign -s -) so Apple silicon runs it; no Apple Developer
# account is involved and the binaries are not notarized.
#
# Running this script locally is the dry run of a release: it never uploads.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"

log() { printf '[package] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }
usage() { sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

WANT_VERSION=""
OUT=""
BUILD=1
SIGN=1
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --version) [ $# -ge 2 ] || die "--version needs a value"; WANT_VERSION="$2"; shift 2 ;;
        --out) [ $# -ge 2 ] || die "--out needs a value"; OUT="$2"; shift 2 ;;
        --no-build) BUILD=0; shift ;;
        --no-sign) SIGN=0; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option $1 (try --help)" ;;
    esac
done

# --- version -------------------------------------------------------------------
# The workspace version is the single source of truth; a tag that disagrees
# with it must fail loudly, here and in the release workflow.
CARGO_VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\([^"]*\)".*/\1/p;}' Cargo.toml | head -n1)"
[ -n "$CARGO_VERSION" ] || die "cannot read [workspace.package] version from Cargo.toml"
VERSION="${WANT_VERSION#v}"
if [ -z "$VERSION" ]; then
    VERSION="$CARGO_VERSION"
elif [ "$VERSION" != "$CARGO_VERSION" ]; then
    die "requested version $VERSION does not match the Cargo workspace version $CARGO_VERSION (bump [workspace.package] version in Cargo.toml, or tag v$CARGO_VERSION)"
fi

# --- platform ------------------------------------------------------------------
case "$(uname -s)" in
    Darwin) OS=darwin ;;
    Linux) OS=linux ;;
    *) die "unsupported operating system $(uname -s); release tarballs are built for macOS and Linux" ;;
esac
case "$(uname -m)" in
    arm64|aarch64) ARCH=arm64; RUST_ARCH=aarch64 ;;
    x86_64|amd64) ARCH=x86_64; RUST_ARCH=x86_64 ;;
    *) die "unsupported architecture $(uname -m)" ;;
esac
if [ "$OS" = darwin ] && [ "$ARCH" != arm64 ]; then
    die "Warren ships macOS binaries for Apple silicon only (the mac instance type needs it); build from source on Intel Macs"
fi
NAME="warren-$VERSION-$OS-$ARCH"
TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"
export CARGO_TARGET_DIR="$TARGET_DIR"
# The default lands in the (git-ignored) target directory; the workflow passes --out dist.
[ -n "$OUT" ] || OUT="$TARGET_DIR/dist"
case "$OUT" in /*) ;; *) OUT="$ROOT/$OUT" ;; esac
log "packaging $NAME (cargo target dir: $TARGET_DIR)"

for tool in tar gzip install; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done
sha256_of() { # file -> hex digest
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d' ' -f1
    else
        die "sha256sum or shasum is required"
    fi
}

# --- build ---------------------------------------------------------------------
GATEWAY=""
if [ "$OS" = linux ]; then
    MUSL="${WARREN_MUSL_TARGET:-$RUST_ARCH-unknown-linux-musl}"
    GATEWAY="$TARGET_DIR/$MUSL/release/warren-gateway"
else
    GATEWAY="$TARGET_DIR/release/warren-gateway"
fi

if [ "$BUILD" = 1 ]; then
    command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH (install rustup, or add ~/.cargo/bin to PATH)"
    log "cargo build --release --locked -p warrend -p warren-cli -p warren-mcp"
    cargo build --release --locked -p warrend -p warren-cli -p warren-mcp
    if [ "$OS" = linux ]; then
        # Mirrors the gateway build in .github/workflows/ci.yml and the Makefile:
        # the musl C compiler wrapper from musl-tools links the C parts (TLS, zlib).
        command -v musl-gcc >/dev/null 2>&1 || die "musl-gcc not found; install musl-tools (make musl-setup)"
        rustup target list --installed 2>/dev/null | grep -qx "$MUSL" \
            || die "Rust target $MUSL is not installed; run: rustup target add $MUSL"
        cc_var="CC_$(printf '%s' "$MUSL" | tr '-' '_')"
        log "cargo build --release --locked -p warren-gateway --target $MUSL ($cc_var=musl-gcc)"
        env "$cc_var=musl-gcc" cargo build --release --locked -p warren-gateway --target "$MUSL"
    else
        log "cargo build --release --locked -p warren-gateway (native macOS build, placed in mac VMs)"
        cargo build --release --locked -p warren-gateway
    fi
fi

BINS="warrend warren warren-mcp"
for b in $BINS; do
    [ -x "$TARGET_DIR/release/$b" ] || die "$TARGET_DIR/release/$b is missing (run without --no-build)"
done
[ -x "$GATEWAY" ] || die "$GATEWAY is missing (run without --no-build)"

# The daemon injects the gateway into arbitrary images; on Linux it must not
# depend on the image's libc.
if [ "$OS" = linux ]; then
    if command -v file >/dev/null 2>&1 && file "$GATEWAY" | grep -q 'dynamically linked'; then
        die "$GATEWAY is dynamically linked; the gateway must be a static musl build"
    fi
    if ldd "$GATEWAY" 2>/dev/null | grep -q '=>'; then
        die "$GATEWAY has dynamic dependencies; the gateway must be a static musl build"
    fi
    log "gateway is statically linked"
fi

# --- stage ---------------------------------------------------------------------
mkdir -p "$OUT"
STAGE="$OUT/$NAME"
rm -rf "$STAGE"
mkdir -p "$STAGE"
for b in $BINS; do
    install -m 0755 "$TARGET_DIR/release/$b" "$STAGE/$b"
done
install -m 0755 "$GATEWAY" "$STAGE/warren-gateway"
install -m 0644 "$ROOT/LICENSE" "$STAGE/LICENSE"

if [ "$OS" = darwin ] && [ "$SIGN" = 1 ]; then
    # Apple silicon refuses to run unsigned arm64 code. An ad-hoc signature
    # (identity "-") is enough for that and needs no certificate, Apple ID or
    # Developer account; strip and install above invalidated the linker's own
    # ad-hoc signature, so sign again. Not notarized: a tarball that arrives
    # through a browser carries the quarantine attribute and Gatekeeper asks.
    command -v codesign >/dev/null 2>&1 || die "codesign not found (Xcode command line tools)"
    for b in $BINS warren-gateway; do
        codesign -s - --force --timestamp=none "$STAGE/$b"
        codesign --verify --strict "$STAGE/$b" || die "codesign verification failed for $b"
    done
    log "ad-hoc signed: $BINS warren-gateway"
fi

# --- smoke test ----------------------------------------------------------------
# The binaries were built for this machine, so they must run here and report
# the version being packaged. warren-mcp reads its settings before parsing
# flags and refuses to start without WARREN_API_KEY, so it is only checked for
# being executable.
for b in warrend warren warren-gateway; do
    got="$("$STAGE/$b" --version 2>&1 | head -n1)" || die "$b --version failed: $got"
    case "$got" in
        "$b $VERSION") ;;
        *) die "$b --version printed '$got', expected '$b $VERSION'" ;;
    esac
done
[ -x "$STAGE/warren-mcp" ] || die "warren-mcp is not executable"
log "binaries run and report version $VERSION"

# --- tarball -------------------------------------------------------------------
TARBALL="$OUT/$NAME.tar.gz"
rm -f "$TARBALL" "$TARBALL.sha256"
# Root-owned entries and a timestamp-free gzip header, so the same inputs give
# the same bytes. bsdtar (macOS) and GNU tar (Linux) spell the options differently.
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
    (cd "$OUT" && tar --owner=0 --group=0 --numeric-owner -cf - "$NAME") | gzip -9n > "$TARBALL"
else
    (cd "$OUT" && COPYFILE_DISABLE=1 tar --uid 0 --gid 0 --uname root --gname root -cf - "$NAME") | gzip -9n > "$TARBALL"
fi
SUM="$(sha256_of "$TARBALL")"
printf '%s  %s\n' "$SUM" "$NAME.tar.gz" > "$TARBALL.sha256"

# Make sure what went in is what comes out.
want="$NAME/LICENSE $NAME/warren $NAME/warren-gateway $NAME/warren-mcp $NAME/warrend"
have="$(tar -tzf "$TARBALL" | grep -v '/$' | sort | tr '\n' ' ' | sed 's/ $//')"
[ "$have" = "$want" ] || die "tarball contents are '$have', expected '$want'"

[ "$KEEP" = 1 ] || rm -rf "$STAGE"

size_bytes="$(wc -c < "$TARBALL" | tr -d ' ')"
log "wrote $TARBALL ($(( (size_bytes + 524288) / 1048576 )) MB)"
log "sha256 $SUM"
echo "$TARBALL"
