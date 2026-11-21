#!/usr/bin/env bash
# Build the release binaries and the static gateway (make build), build the
# template images (make images) and run the end-to-end suite (crates/warren-e2e)
# against a real warrend and Docker.
#
# Usage: scripts/e2e.sh [--vm] [--no-build] [--no-images] [--keep] [-- <test filter and cargo test args>]
#   --vm         also run the vm_* scenario: sets WARREN_E2E_VM=1 and runs the
#                tests under sudo (Firecracker needs root and /dev/kvm)
#   --no-build   do not run make build (use the binaries already there)
#   --no-images  do not rebuild the template images
#   --keep       keep every daemon's data directory and log (WARREN_E2E_KEEP=1)
#   -- ...       passed to the test binary, e.g. `-- lifecycle` runs one test
#
# Environment:
#   WARREN_BIN                 daemon binary (default $CARGO_TARGET_DIR/release/warrend)
#   WARREN_GATEWAY_BIN         static (musl) warren-gateway every daemon the suite starts gets as
#                              --gateway-bin and injects into registered images (default
#                              $CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/warren-gateway,
#                              built by make build); exported to the suite and staged into the images
#   WARREN_IMAGE_TAG           image tag (default dev); must match the daemon's --image-tag
#   WARREN_E2E_IMAGES          build only these images with scripts/build-images.sh instead of all
#                              three with make images, e.g. "base agent"; the daemon seeds all three
#                              system templates, so build all unless you know the templates service
#                              tolerates a missing image
#   WARREN_E2E_LOG             daemon log filter (default info)
#   WARREN_E2E_IMAGE_TAG       set automatically to WARREN_IMAGE_TAG so the daemons use the images just built
#   WARREN_E2E_KERNEL          kernel for --vm (default /data/sandbox-probe/vmlinux-6.1.155)
#   WARREN_E2E_FIRECRACKER_BIN firecracker binary for --vm (default /data/sandbox-probe/firecracker)
#   WARREN_E2E_FIRECRACKER_JAILER  jailer binary: when set, the vm daemons run with --firecracker-jailer
#
# make build and make images need the musl toolchain for the static gateway
# (once: make musl-setup).
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
    log "building the release binaries and the static gateway (make build) into $CARGO_TARGET_DIR"
    make -C "$ROOT" build
fi
BIN="${WARREN_BIN:-$CARGO_TARGET_DIR/release/warrend}"
[ -x "$BIN" ] || die "$BIN not found; run without --no-build, run 'make build', or set WARREN_BIN"
export WARREN_BIN="$BIN"
GATEWAY="${WARREN_GATEWAY_BIN:-$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/warren-gateway}"
[ -x "$GATEWAY" ] || die "$GATEWAY not found; run without --no-build, run 'make gateway' (once: make musl-setup), or set WARREN_GATEWAY_BIN"
export WARREN_GATEWAY_BIN="$GATEWAY"
log "daemon binary: $WARREN_BIN"
log "static gateway: $WARREN_GATEWAY_BIN (passed to every daemon as --gateway-bin)"

# --- images ---------------------------------------------------------------------
# The images carry the same static gateway the daemons inject: WARREN_GATEWAY_BIN
# is exported, so build-images.sh stages it instead of compiling again.
if [ "$IMAGES" = 1 ]; then
    if [ -n "${WARREN_E2E_IMAGES:-}" ]; then
        # shellcheck disable=SC2086  # WARREN_E2E_IMAGES is a space-separated list
        set -- $WARREN_E2E_IMAGES
        log "building images: $* (tag $TAG)"
        WARREN_IMAGE_TAG="$TAG" "$ROOT/scripts/build-images.sh" "$@"
    else
        log "building the template images (make images, tag $TAG)"
        WARREN_IMAGE_TAG="$TAG" make -C "$ROOT" images
    fi
fi
for img in base agent; do
    docker image inspect "warren/$img:$TAG" >/dev/null 2>&1 || die "warren/$img:$TAG is missing; run without --no-images or scripts/build-images.sh"
done
# The templates test registers workspace templates from these images (the daemon
# pulls on registration; pulling here keeps that step quick).
for img in alpine:3.20 node:24-bookworm-slim; do
    docker image inspect "$img" >/dev/null 2>&1 || docker pull "$img"
done

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
        "CARGO_TARGET_DIR=$CARGO_TARGET_DIR" "WARREN_BIN=$WARREN_BIN" "WARREN_GATEWAY_BIN=$WARREN_GATEWAY_BIN" \
        "WARREN_E2E_VM=1" "WARREN_E2E_IMAGE_TAG=$TAG" "WARREN_E2E_LOG=${WARREN_E2E_LOG:-info}" \
        "WARREN_E2E_KERNEL=${WARREN_E2E_KERNEL:-}" "WARREN_E2E_FIRECRACKER_BIN=${WARREN_E2E_FIRECRACKER_BIN:-}" \
        "WARREN_E2E_FIRECRACKER_JAILER=${WARREN_E2E_FIRECRACKER_JAILER:-}" "${CMD[@]}"
fi
log "running the suite (vm_create_exec is skipped without --vm)"
exec "${CMD[@]}"
