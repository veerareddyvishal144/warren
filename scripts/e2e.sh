#!/usr/bin/env bash
# Build the release binaries, build the template images and run the end-to-end
# suite (crates/warren-e2e) against a real warrend and Docker.
#
# Usage: scripts/e2e.sh [--vm] [--no-build] [--no-images] [--keep] [-- <test filter and cargo test args>]
#   --vm         also run the vm_* scenario: sets WARREN_E2E_VM=1 and runs the
#                tests under sudo (Firecracker needs root and /dev/kvm)
#   --no-build   do not run cargo build --release (use the binaries already there)
#   --no-images  do not rebuild the template images
#   --keep       keep every daemon's data directory and log (WARREN_E2E_KEEP=1)
#   -- ...       passed to the test binary, e.g. `-- lifecycle` runs one test
#
# Environment:
#   WARREN_BIN                 daemon binary (default $CARGO_TARGET_DIR/release/warrend)
#   WARREN_IMAGE_TAG           image tag (default dev); must match the daemon's --image-tag
#   WARREN_E2E_IMAGES          images to build (default "base agent opencode"); the daemon
#                              seeds all three system templates, so build all unless you know
#                              the templates service tolerates a missing image
#   WARREN_E2E_LOG             daemon log filter (default info)
#   WARREN_E2E_IMAGE_TAG       set automatically to WARREN_IMAGE_TAG so the daemons use the images just built
#   WARREN_E2E_KERNEL          kernel for --vm (default /data/sandbox-probe/vmlinux-6.1.155)
#   WARREN_E2E_FIRECRACKER_BIN firecracker binary for --vm (default /data/sandbox-probe/firecracker)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/env.sh
source "$ROOT/scripts/env.sh"
cd "$ROOT"

log() { printf '[e2e] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

VM=0 BUILD=1 IMAGES=1
FILTER=()
while [ $# -gt 0 ]; do
    case "$1" in
        --vm) VM=1 ;;
        --no-build) BUILD=0 ;;
        --no-images) IMAGES=0 ;;
        --keep) export WARREN_E2E_KEEP=1 ;;
        --) shift; FILTER=("$@"); break ;;
        -h|--help) sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1 (try --help)" ;;
    esac
    shift
done

command -v docker >/dev/null 2>&1 || die "docker is not installed or not on PATH"
docker info >/dev/null 2>&1 || die "cannot talk to the Docker daemon"

TAG="${WARREN_IMAGE_TAG:-dev}"
# The daemons the harness starts use the same tag (--image-tag).
export WARREN_E2E_IMAGE_TAG="$TAG"

# --- binaries -----------------------------------------------------------------
if [ "$BUILD" = 1 ]; then
    log "building warrend and warren-gateway (release) into $CARGO_TARGET_DIR"
    cargo build --release --locked -p warrend -p warren-gateway
fi
BIN="${WARREN_BIN:-$CARGO_TARGET_DIR/release/warrend}"
[ -x "$BIN" ] || die "$BIN not found; run without --no-build, run 'make build', or set WARREN_BIN"
export WARREN_BIN="$BIN"
log "daemon binary: $WARREN_BIN"

# --- images ---------------------------------------------------------------------
if [ "$IMAGES" = 1 ]; then
    GATEWAY=""
    for candidate in "$CARGO_TARGET_DIR/release/warren-gateway" "$CARGO_TARGET_DIR/release/gateway"; do
        if [ -x "$candidate" ]; then GATEWAY="$candidate"; break; fi
    done
    # shellcheck disable=SC2086  # WARREN_E2E_IMAGES is a space-separated list
    set -- ${WARREN_E2E_IMAGES:-base agent opencode}
    log "building images: $* (tag $TAG)"
    if [ -n "$GATEWAY" ]; then
        WARREN_GATEWAY_BIN="$GATEWAY" WARREN_IMAGE_TAG="$TAG" "$ROOT/scripts/build-images.sh" "$@"
    else
        WARREN_IMAGE_TAG="$TAG" "$ROOT/scripts/build-images.sh" "$@"
    fi
fi
for img in base agent; do
    docker image inspect "warren/$img:$TAG" >/dev/null 2>&1 || die "warren/$img:$TAG is missing; run without --no-images or scripts/build-images.sh"
done
# The templates test registers a workspace template from this image.
docker image inspect alpine:3.20 >/dev/null 2>&1 || docker pull alpine:3.20

# --- run ------------------------------------------------------------------------
CMD=(cargo test --locked -p warren-e2e -- --ignored --test-threads=1 --nocapture)
if [ ${#FILTER[@]} -gt 0 ]; then
    CMD+=("${FILTER[@]}")
fi

if [ "$VM" = 1 ]; then
    [ -e /dev/kvm ] || die "/dev/kvm is missing; the vm scenario needs KVM"
    export WARREN_E2E_VM=1
    log "running the suite including vm_* under sudo"
    exec sudo -E env "PATH=$PATH" "CARGO_HOME=$CARGO_HOME" "RUSTUP_HOME=$RUSTUP_HOME" "TMPDIR=$TMPDIR" \
        "CARGO_TARGET_DIR=$CARGO_TARGET_DIR" "WARREN_BIN=$WARREN_BIN" "WARREN_E2E_VM=1" \
        "WARREN_E2E_IMAGE_TAG=$TAG" "WARREN_E2E_LOG=${WARREN_E2E_LOG:-info}" "${CMD[@]}"
fi
log "running the suite (vm_create_exec is skipped without --vm)"
exec "${CMD[@]}"
