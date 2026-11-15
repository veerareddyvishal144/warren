#!/usr/bin/env bash
# Build warrend (release) and run it in dev mode with data under ./data.
#
# Usage: scripts/dev-up.sh [--no-build] [-- <extra warrend flags>]
#   --no-build   run the binary already in $CARGO_TARGET_DIR/release (or $WARREN_BIN)
#
# Environment:
#   WARREN_PORT         listen and public port (default 7337)
#   WARREN_DATA_DIR     data directory (default ./data: sqlite, secret, instance homes, backups)
#   WARREN_BASE_DOMAIN  base domain (default warren.localhost)
#   WARREN_LOG          log filter (default info)
#   WARREN_IMAGE_TAG    tag of the built-in images (default dev)
#   WARREN_BIN          daemon binary (default $CARGO_TARGET_DIR/release/warrend)
#
# Dev mode means: dev top-ups through POST /v1/billing/top-up, a 5 s minimum
# idle timeout and host.docker.internal for instances. Stop with Ctrl-C.
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

if [ "$BUILD" = 1 ]; then
    log "building warrend (release) into $CARGO_TARGET_DIR"
    cargo build --release --locked -p warrend
fi
BIN="${WARREN_BIN:-$CARGO_TARGET_DIR/release/warrend}"
[ -x "$BIN" ] || die "$BIN not found; run without --no-build or set WARREN_BIN"

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
    "${EXTRA[@]}"
