#!/bin/sh
# Entrypoint of the Warren daemon image (Dockerfile at the repository root).
#
# With `warrend` as the command (the default) it checks the two mounts the
# daemon cannot do without, places the gateway where dockerd can find it, picks
# the iptables backend, and execs warrend. Any other command runs as is, so
# `docker run --rm warren:local warren --help` works.
#
#   WARREN_DATA_DIR    data directory, default /var/lib/warren; must be bind-mounted
#                      from the same path on the host (dockerd resolves the paths the
#                      daemon asks it to mount on the host, not in this container)
#   WARREN_GATEWAY_BIN set to skip the copy below and use that path instead
#   WARREN_IPTABLES    legacy: use iptables-legacy (hosts whose dockerd uses the
#                      legacy tables; `iptables -S DOCKER-USER` on the host tells which
#                      backend has the chain). Default: nft, Debian's default.
#   WARREN_RUNTIME     none: no Docker socket required (nothing useful in a container)
set -eu

log() { printf '[warren-entrypoint] %s\n' "$*" >&2; }

case "${1:-warrend}" in
    warrend) ;;
    *) exec "$@" ;;
esac
[ $# -gt 0 ] || set -- warrend

data_dir="${WARREN_DATA_DIR:-/var/lib/warren}"

if [ "${WARREN_RUNTIME:-auto}" != none ] && [ ! -S /var/run/docker.sock ]; then
    log "error: /var/run/docker.sock is not mounted; run with -v /var/run/docker.sock:/var/run/docker.sock (see packaging/docker-compose.yml)"
    exit 1
fi

mkdir -p "$data_dir"
if command -v mountpoint >/dev/null 2>&1 && ! mountpoint -q "$data_dir"; then
    log "warning: $data_dir is not a mount; instance homes would live only in this container and dockerd could not bind-mount them. Mount the host directory at the same path: -v $data_dir:$data_dir"
fi

# The daemon bind-mounts the gateway binary from a host path into every
# instance that lacks it. The only path this container shares with the host
# under the same name is the data directory, so the gateway is copied there.
if [ -z "${WARREN_GATEWAY_BIN:-}" ]; then
    gw_dir="$data_dir/bin"
    mkdir -p "$gw_dir"
    if ! cmp -s /usr/local/bin/warren-gateway "$gw_dir/warren-gateway" 2>/dev/null; then
        cp /usr/local/bin/warren-gateway "$gw_dir/warren-gateway.tmp.$$"
        chmod 0755 "$gw_dir/warren-gateway.tmp.$$"
        mv -f "$gw_dir/warren-gateway.tmp.$$" "$gw_dir/warren-gateway"
        log "placed warren-gateway in $gw_dir"
    fi
    WARREN_GATEWAY_BIN="$gw_dir/warren-gateway"
    export WARREN_GATEWAY_BIN
fi

if [ "${WARREN_IPTABLES:-nft}" = legacy ]; then
    update-alternatives --set iptables /usr/sbin/iptables-legacy >/dev/null
    update-alternatives --set ip6tables /usr/sbin/ip6tables-legacy >/dev/null
    log "using iptables-legacy"
fi

exec "$@"
