#!/bin/bash
# Starts warren-gateway inside a Warren macOS guest. Run by launchd
# (dev.warren.gateway.plist) as the user `lume`; restarted on every exit.
#
# The host (warrend) attaches a read-only virtiofs folder named `share` to the
# VM, which macOS automounts under "/Volumes/My Shared Files/share". It holds
# the gateway binary (arm64 macOS build) and `warren.env`, a POSIX sh fragment
# of `export KEY='value'` lines (instance id, token, driver, model, ...).
#
# This script waits for both files, copies the binary into the home when it
# differs from the installed one, sources the environment and execs the
# gateway. Exit 1 when the share never shows up; launchd starts it again.
set -euo pipefail

SHARE_DIR="/Volumes/My Shared Files/share"
SHARE_BIN="$SHARE_DIR/warren-gateway"
SHARE_ENV="$SHARE_DIR/warren.env"
HOME_DIR="/Users/lume"
INSTALL_DIR="$HOME_DIR/.warren"
INSTALL_BIN="$INSTALL_DIR/warren-gateway"
WAIT_SECONDS=300

log() { printf '%s warren-gateway-launcher: %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$*" >&2; }

log "waiting up to ${WAIT_SECONDS}s for $SHARE_BIN and $SHARE_ENV"
waited=0
while [ ! -f "$SHARE_BIN" ] || [ ! -f "$SHARE_ENV" ]; do
    if [ "$waited" -ge "$WAIT_SECONDS" ]; then
        log "error: the share did not appear within ${WAIT_SECONDS}s (is this VM started by warrend with a 'share' folder attached?)"
        [ -d "$SHARE_DIR" ] && ls -la "$SHARE_DIR" >&2
        exit 1
    fi
    sleep 1
    waited=$((waited + 1))
done
log "share present after ${waited}s"

mkdir -p "$INSTALL_DIR"

# Install the binary only when it changed. Copy to a temporary name and rename
# so the file at INSTALL_BIN is never half written.
if [ -f "$INSTALL_BIN" ] && cmp -s "$SHARE_BIN" "$INSTALL_BIN"; then
    log "gateway binary unchanged at $INSTALL_BIN"
else
    log "installing gateway binary from the share"
    tmp="$INSTALL_BIN.tmp.$$"
    if ! cp -f "$SHARE_BIN" "$tmp"; then
        log "error: could not copy $SHARE_BIN (check the file is readable by the lume user)"
        rm -f "$tmp"
        exit 1
    fi
    chmod 755 "$tmp"
    mv -f "$tmp" "$INSTALL_BIN"
fi
chmod 755 "$INSTALL_BIN"

log "sourcing $SHARE_ENV"
# The file is written by the daemon as `export KEY='value'` lines; `set -u` is
# relaxed while sourcing so a stray reference to an unset variable cannot kill us.
set +u
# shellcheck disable=SC1090
. "$SHARE_ENV"
set -u

export WARREN_HOME="${WARREN_HOME:-$HOME_DIR}"
export HOME="${HOME:-$HOME_DIR}"
cd "$HOME_DIR"

log "starting $INSTALL_BIN --serve (instance ${WARREN_INSTANCE_ID:-unknown}, WARREN_HOME=$WARREN_HOME)"
exec "$INSTALL_BIN" --serve
