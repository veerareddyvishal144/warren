#!/usr/bin/env bash
# Build warrend (release) and the static gateway, then run warrend in dev mode
# with data under ./data.
#
# Usage: scripts/dev-up.sh [--no-build] [-- <extra warrend flags>]
#   --no-build   run the binaries already there: $CARGO_TARGET_DIR/release/warrend
#                (or $WARREN_BIN) and the static gateway (see WARREN_GATEWAY_BIN)
#
# Environment:
#   WARREN_PORT         listen and public port (default 7337)
#   WARREN_DATA_DIR     data directory (default ./data: sqlite, secret, instance homes, backups)
#   WARREN_BASE_DOMAIN  base domain (default warren.localhost)
#   WARREN_LOG          log filter (default info)
#   WARREN_IMAGE_TAG    tag of the built-in images (default dev)
#   WARREN_BIN          daemon binary (default $CARGO_TARGET_DIR/release/warrend)
#   WARREN_GATEWAY_BIN  static (musl) warren-gateway the daemon injects into registered
#                       images, passed as --gateway-bin (default
#                       $CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/warren-gateway,
#                       built with `make gateway` when missing; `make musl-setup` once)
#   WARREN_MAX_AWAKE_INSTANCES
#                       instances a workspace may keep awake at once, passed as
#                       --max-awake-instances (unset: unlimited); the lifecycle answers
#                       409 instance_limit_reached beyond it
#
# Dev mode means: a 5 s minimum idle timeout and host.docker.internal for
# instances. Stop with Ctrl-C.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/env.sh
source "$ROOT/scripts/env.sh"
cd "$ROOT"

log() { printf '[dev-up] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

BUILD=1
EXTRA=()
while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) BUILD=0 ;;
        --) shift; EXTRA=("$@"); break ;;
        -h|--help) sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1 (try --help)" ;;
    esac
    shift
done

PORT="${WARREN_PORT:-7337}"
DATA="${WARREN_DATA_DIR:-$ROOT/data}"
BASE="${WARREN_BASE_DOMAIN:-warren.localhost}"
TAG="${WARREN_IMAGE_TAG:-dev}"
GATEWAY="${WARREN_GATEWAY_BIN:-$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/warren-gateway}"

if [ "$BUILD" = 1 ]; then
    log "building warrend (release) into $CARGO_TARGET_DIR"
    cargo build --release --locked -p warrend
    if [ -z "${WARREN_GATEWAY_BIN:-}" ] && [ ! -x "$GATEWAY" ]; then
        log "building the static gateway (make gateway)"
        make -C "$ROOT" gateway
    fi
fi
BIN="${WARREN_BIN:-$CARGO_TARGET_DIR/release/warrend}"
[ -x "$BIN" ] || die "$BIN not found; run without --no-build or set WARREN_BIN"

# The daemon injects this binary into registered images that do not ship the
# gateway; without it such instances cannot start (the daemon warns at startup).
GATEWAY_FLAGS=()
if [ -x "$GATEWAY" ]; then
    GATEWAY_FLAGS=(--gateway-bin "$GATEWAY")
elif [ -n "${WARREN_GATEWAY_BIN:-}" ]; then
    die "WARREN_GATEWAY_BIN=$WARREN_GATEWAY_BIN is not an executable file"
else
    log "warning: no static gateway at $GATEWAY; registered images that do not ship the gateway will not start (run 'make gateway' or set WARREN_GATEWAY_BIN)"
fi

# The only capacity control: unset means unlimited awake instances per workspace.
LIMIT_FLAGS=()
if [ -n "${WARREN_MAX_AWAKE_INSTANCES:-}" ]; then
    case "$WARREN_MAX_AWAKE_INSTANCES" in
        ''|*[!0-9]*) die "WARREN_MAX_AWAKE_INSTANCES=$WARREN_MAX_AWAKE_INSTANCES is not a whole number" ;;
    esac
    LIMIT_FLAGS=(--max-awake-instances "$WARREN_MAX_AWAKE_INSTANCES")
fi

command -v docker >/dev/null 2>&1 || die "docker is not installed or not on PATH"
docker info >/dev/null 2>&1 || die "cannot talk to the Docker daemon"
if ! docker image inspect "warren/base:$TAG" >/dev/null 2>&1; then
    log "warning: warren/base:$TAG is not built; run scripts/build-images.sh (or make images) before creating instances"
fi

mkdir -p "$DATA"
echo
echo "  Dashboard:  http://$BASE:$PORT/"
echo "  API:        http://api.$BASE:$PORT/v1/health"
echo "  Instances:  http://<id>.$BASE:$PORT/   (preview ports: http://<id>-<port>.$BASE:$PORT/)"
echo "  Data:       $DATA"
if [ ${#GATEWAY_FLAGS[@]} -gt 0 ]; then
    echo "  Gateway:    $GATEWAY   (injected into registered images)"
fi
if [ ${#LIMIT_FLAGS[@]} -gt 0 ]; then
    echo "  Awake limit: $WARREN_MAX_AWAKE_INSTANCES instances per workspace (--max-awake-instances)"
else
    echo "  Awake limit: unlimited (set WARREN_MAX_AWAKE_INSTANCES to cap awake instances per workspace)"
fi
echo
echo "  First steps: curl -s -X POST http://api.$BASE:$PORT/v1/signup | jq ."
echo "  See docs/TESTING.md for a curl walkthrough of every feature."
echo

exec "$BIN" --dev \
    --listen "[::]:$PORT" \
    --public-port "$PORT" \
    --data-dir "$DATA" \
    --base-domain "$BASE" \
    --image-tag "$TAG" \
    --log "${WARREN_LOG:-info}" \
    "${GATEWAY_FLAGS[@]}" \
    "${LIMIT_FLAGS[@]}" \
    "${EXTRA[@]}"
