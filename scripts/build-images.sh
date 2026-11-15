#!/usr/bin/env bash
# Build the built-in template images: warren/base, warren/agent, warren/opencode.
#
# Usage:
#   scripts/build-images.sh [base|agent|opencode ...]     (default: all three, in that order)
#
# Environment:
#   WARREN_IMAGE_TAG            tag for every image (default: dev); the daemon's image_tag must match
#   WARREN_GATEWAY_BIN          stage this prebuilt gateway binary instead of running cargo build
#   WARREN_SKIP_GATEWAY_BUILD=1 reuse the binary already staged at images/.build/warren-gateway
#   WARREN_TTYD_VERSION         ttyd release for the base image (default: latest)
#   WARREN_OPENCODE_VERSION     opencode-ai npm version or dist-tag for the opencode image (default: latest)
#   WARREN_DOCKER_BUILD_ARGS    extra flags for every docker build, e.g. "--pull --no-cache"
#   WARREN_BASE_SIZE_LIMIT_MB   size budget of warren/base (default: 700); exceeding it prints a warning
#
# The gateway is built with the default CARGO_TARGET_DIR from scripts/env.sh
# (/data/warren/target unless overridden). The docker build context is the
# images/ directory, so the base Dockerfile copies .build/warren-gateway, i.e.
# images/.build/warren-gateway in the repository (git-ignored).
#
# Docker caches the "latest" downloads (ttyd, opencode-ai) in image layers;
# pass WARREN_DOCKER_BUILD_ARGS=--no-cache to refresh them.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
# shellcheck source=scripts/env.sh
source "$ROOT/scripts/env.sh"

TAG="${WARREN_IMAGE_TAG:-dev}"
IMAGES_DIR="$ROOT/images"
STAGED="$IMAGES_DIR/.build/warren-gateway"
SIZE_LIMIT_MB="${WARREN_BASE_SIZE_LIMIT_MB:-700}"
ALL_IMAGES=(base agent opencode)

log() { printf '[build-images] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

# --- which images -----------------------------------------------------------
if [ $# -gt 0 ]; then
    SELECTED=("$@")
else
    SELECTED=("${ALL_IMAGES[@]}")
fi
for name in "${SELECTED[@]}"; do
    case "$name" in
        base|agent|opencode) [ -f "$IMAGES_DIR/$name/Dockerfile" ] || die "missing $IMAGES_DIR/$name/Dockerfile" ;;
        *) die "unknown image '$name' (expected: ${ALL_IMAGES[*]})" ;;
    esac
done

command -v docker >/dev/null 2>&1 || die "docker is not installed or not on PATH"
docker info >/dev/null 2>&1 || die "cannot talk to the Docker daemon (is it running, are you in the docker group?)"

builds_base=0
for name in "${SELECTED[@]}"; do [ "$name" = base ] && builds_base=1; done

# --- stage the gateway binary -------------------------------------------------
# Only the base image copies the binary; derived images inherit it.
if [ "$builds_base" = 1 ]; then
    mkdir -p "$IMAGES_DIR/.build"
    if [ -n "${WARREN_GATEWAY_BIN:-}" ]; then
        [ -f "$WARREN_GATEWAY_BIN" ] || die "WARREN_GATEWAY_BIN=$WARREN_GATEWAY_BIN does not exist"
        log "staging prebuilt gateway from $WARREN_GATEWAY_BIN"
        install -m 0755 "$WARREN_GATEWAY_BIN" "$STAGED"
    elif [ "${WARREN_SKIP_GATEWAY_BUILD:-0}" = 1 ]; then
        [ -f "$STAGED" ] || die "WARREN_SKIP_GATEWAY_BUILD=1 but $STAGED is missing"
        log "reusing staged gateway at $STAGED"
    else
        command -v cargo >/dev/null 2>&1 || die "cargo not found; source scripts/env.sh or set WARREN_GATEWAY_BIN"
        log "building warren-gateway (release, static musl) into $CARGO_TARGET_DIR"
        (cd "$ROOT" && CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --locked -p warren-gateway --target x86_64-unknown-linux-musl)
        # The crate's [[bin]] may be named warren-gateway or gateway; accept either.
        built=""
        for candidate in "$CARGO_TARGET_DIR/x86_64-unknown-linux-musl/release/warren-gateway" "$CARGO_TARGET_DIR/release/warren-gateway"; do
            if [ -f "$candidate" ] && [ -x "$candidate" ]; then
                built="$candidate"
                break
            fi
        done
        [ -n "$built" ] || die "cargo build succeeded but no gateway binary found under $CARGO_TARGET_DIR/release (looked for warren-gateway and gateway)"
        log "staging $built"
        install -m 0755 "$built" "$STAGED"
    fi
    if head -c 4 "$STAGED" | grep -q 'ELF'; then
        log "gateway binary: $(stat -c '%s' "$STAGED") bytes (ELF)"
    else
        log "warning: $STAGED is not an ELF binary (stand-in script?)"
    fi
fi

# --- build -------------------------------------------------------------------
# shellcheck disable=SC2206  # intentional word splitting of user-supplied flags
extra_args=(${WARREN_DOCKER_BUILD_ARGS:-})

build_one() {
    local name="$1" ref="warren/$1:$TAG"
    local -a args=(--build-arg "WARREN_IMAGE_TAG=$TAG")
    case "$name" in
        base)
            [ -n "${WARREN_TTYD_VERSION:-}" ] && args+=(--build-arg "TTYD_VERSION=$WARREN_TTYD_VERSION")
            ;;
        agent|opencode)
            docker image inspect "warren/base:$TAG" >/dev/null 2>&1 \
                || die "warren/base:$TAG is not available locally; build it first (scripts/build-images.sh base)"
            if [ "$name" = opencode ] && [ -n "${WARREN_OPENCODE_VERSION:-}" ]; then
                args+=(--build-arg "OPENCODE_VERSION=$WARREN_OPENCODE_VERSION")
            fi
            ;;
    esac
    log "building $ref"
    docker build "${extra_args[@]}" "${args[@]}" -f "$IMAGES_DIR/$name/Dockerfile" -t "$ref" "$IMAGES_DIR"
}

for name in "${SELECTED[@]}"; do
    build_one "$name"
done

# --- report ------------------------------------------------------------------
size_mb() { # decimal MB, like docker reports
    local bytes
    bytes="$(docker image inspect -f '{{.Size}}' "$1")"
    echo $(( (bytes + 500000) / 1000000 ))
}

echo
printf '%-28s %10s   %s\n' IMAGE SIZE ID
for name in "${SELECTED[@]}"; do
    ref="warren/$name:$TAG"
    mb="$(size_mb "$ref")"
    id="$(docker image inspect -f '{{.Id}}' "$ref" | cut -c8-19)"
    printf '%-28s %7s MB   %s\n' "$ref" "$mb" "$id"
    if [ "$name" = base ] && [ "$mb" -gt "$SIZE_LIMIT_MB" ]; then
        log "warning: $ref is $mb MB, over the $SIZE_LIMIT_MB MB budget"
    fi
done
