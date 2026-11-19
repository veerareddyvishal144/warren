#!/bin/bash
# Starts warren-gateway inside a Warren macOS guest. Run by launchd
# (dev.warren.gateway.plist) as root; restarted on every exit.
#
# The host (warrend) attaches a read-only virtiofs folder named `share` to the
# VM, which macOS automounts under "/Volumes/My Shared Files/share". It holds
# the gateway binary (arm64 macOS build) and `warren.env`, a POSIX sh fragment
# of `export KEY='value'` lines (instance id, token, driver, model, ...).
#
# This script waits for both files, copies the binary into the home when it
# differs from the installed one, sources the environment and starts the
# gateway as the guest user, inside that user's own launchd domain:
#
#   launchctl asuser <uid> sudo -n -E -u lume -H warren-gateway --serve
#
# Why not `UserName lume` in the plist: a LaunchDaemon's processes live in
# launchd's *system* domain, and Apple's nohup(1) (and anything else calling
# _vprocmgr_detach_from_console) fails there with "can't detach from
# console", so an agent's `nohup server &` would die before it ran. `launchctl
# asuser` moves us into the per-user "Background" domain, the one SSH sessions
# get, where nohup works; sudo then drops root, keeping the environment (-E,
# allowed because root's sudoers rule is ALL) and HOME (-H). launchd keeps
# tracking the job through sudo, which relays signals and exits with the
# gateway's status. Exit 1 when the share never shows up; launchd starts it
# again.
set -euo pipefail

SHARE_DIR="/Volumes/My Shared Files/share"
SHARE_BIN="$SHARE_DIR/warren-gateway"
SHARE_ENV="$SHARE_DIR/warren.env"
GUEST_USER="${WARREN_GUEST_USER:-lume}"
GUEST_GROUP=staff
HOME_DIR="/Users/$GUEST_USER"
INSTALL_DIR="$HOME_DIR/.warren"
INSTALL_BIN="$INSTALL_DIR/warren-gateway"
WAIT_SECONDS=300

log() { printf '%s warren-gateway-launcher: %s\n' "$(date '+%Y-%m-%dT%H:%M:%S%z')" "$*" >&2; }

AS_ROOT=0
[ "$(id -u)" = 0 ] && AS_ROOT=1
GUEST_UID="$(id -u "$GUEST_USER" 2>/dev/null || true)"
if [ -z "$GUEST_UID" ]; then
    log "error: user $GUEST_USER does not exist in this guest"
    exit 1
fi

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
[ "$AS_ROOT" = 1 ] && chown "$GUEST_USER:$GUEST_GROUP" "$INSTALL_DIR"

# Install the binary only when it changed. Copy to a temporary name and rename
# so the file at INSTALL_BIN is never half written.
if [ -f "$INSTALL_BIN" ] && cmp -s "$SHARE_BIN" "$INSTALL_BIN"; then
    log "gateway binary unchanged at $INSTALL_BIN"
else
    log "installing gateway binary from the share"
    tmp="$INSTALL_BIN.tmp.$$"
    if ! cp -f "$SHARE_BIN" "$tmp"; then
        log "error: could not copy $SHARE_BIN (check the file is readable)"
        rm -f "$tmp"
        exit 1
    fi
    chmod 755 "$tmp"
    [ "$AS_ROOT" = 1 ] && chown "$GUEST_USER:$GUEST_GROUP" "$tmp"
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
export HOME="$HOME_DIR"
cd "$HOME_DIR"

if [ "$AS_ROOT" = 1 ]; then
    log "starting $INSTALL_BIN --serve as $GUEST_USER (uid $GUEST_UID) in launchd domain user/$GUEST_UID (instance ${WARREN_INSTANCE_ID:-unknown}, WARREN_HOME=$WARREN_HOME)"
    # env -u drops the variables sudo adds, so commands the gateway runs for
    # the agent do not see SUDO_USER=root and friends.
    exec /bin/launchctl asuser "$GUEST_UID" /usr/bin/sudo -n -E -u "$GUEST_USER" -H -- \
        /usr/bin/env -u SUDO_COMMAND -u SUDO_GID -u SUDO_HOME -u SUDO_UID -u SUDO_USER \
        "$INSTALL_BIN" --serve
fi
log "not root: starting $INSTALL_BIN --serve directly as $(id -un) in launchd domain '$(launchctl managername 2>/dev/null || echo unknown)' (instance ${WARREN_INSTANCE_ID:-unknown}, WARREN_HOME=$WARREN_HOME)"
exec "$INSTALL_BIN" --serve
