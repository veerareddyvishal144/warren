#!/bin/bash
# Installs the Warren guest pieces INSIDE a macOS VM. Run as root (sudo) from
# the directory scripts/mac/build-base.sh copied here (default /tmp/warren-guest).
#
# Usage: sudo bash install.sh --subnet CIDR --gateway IP [--ssh-key "ssh-ed25519 AAAA..."] [--ssh-key-file FILE]
#
# What it does (every step is idempotent):
#   - /usr/local/libexec/warren-gateway-launcher.sh and the two LaunchDaemons
#     dev.warren.gateway (the gateway, as user lume) and dev.warren.pf (enables pf at boot)
#   - /etc/pf.anchors/dev.warren rendered from pf.anchor.template, referenced from /etc/pf.conf, loaded now
#   - /Users/lume/.warren and /Users/lume/Library/Logs owned by lume
#   - the given public key appended to /Users/lume/.ssh/authorized_keys
#   - no system sleep (pmset) and no automatic software updates
#   - both daemons bootstrapped into the system domain
set -euo pipefail

log() { printf '[install] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

[ "$(id -u)" = 0 ] || die "run as root: sudo bash $0 ..."
[ "$(uname -s)" = Darwin ] || die "this installer only runs inside a macOS guest"

SRC="$(cd "$(dirname "$0")" && pwd)"
SUBNET="" GATEWAY="" SSH_KEY="" SSH_KEY_FILE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --subnet) SUBNET="${2:-}"; shift ;;
        --gateway) GATEWAY="${2:-}"; shift ;;
        --ssh-key) SSH_KEY="${2:-}"; shift ;;
        --ssh-key-file) SSH_KEY_FILE="${2:-}"; shift ;;
        -h|--help) sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1 (try --help)" ;;
    esac
    shift
done
[ -n "$SUBNET" ] || die "--subnet CIDR is required (for example 192.168.64.0/24)"
[ -n "$GATEWAY" ] || die "--gateway IP is required (for example 192.168.64.1)"
case "$SUBNET" in
    *[!0-9./]*|'') die "--subnet must be an IPv4 CIDR such as 192.168.64.0/24, got '$SUBNET'" ;;
esac
case "$GATEWAY" in
    *[!0-9.]*|'') die "--gateway must be an IPv4 address such as 192.168.64.1, got '$GATEWAY'" ;;
esac
if [ -n "$SSH_KEY_FILE" ]; then
    [ -f "$SSH_KEY_FILE" ] || die "--ssh-key-file $SSH_KEY_FILE does not exist"
    SSH_KEY="$(head -n 1 "$SSH_KEY_FILE")"
fi

GUEST_USER=lume
GUEST_GROUP=staff
GUEST_HOME=/Users/$GUEST_USER
id "$GUEST_USER" >/dev/null 2>&1 || die "user $GUEST_USER does not exist in this guest"

for f in warren-gateway-launcher.sh dev.warren.gateway.plist dev.warren.pf.plist pf.anchor.template; do
    [ -f "$SRC/$f" ] || die "$SRC/$f is missing; copy the whole scripts/mac/guest directory"
done

# --- launcher and LaunchDaemons -------------------------------------------------
log "installing /usr/local/libexec/warren-gateway-launcher.sh"
install -d -o root -g wheel -m 755 /usr/local/libexec
install -o root -g wheel -m 755 "$SRC/warren-gateway-launcher.sh" /usr/local/libexec/warren-gateway-launcher.sh
bash -n /usr/local/libexec/warren-gateway-launcher.sh

for plist in dev.warren.gateway.plist dev.warren.pf.plist; do
    plutil -lint "$SRC/$plist" >/dev/null || die "$SRC/$plist is not a valid property list"
    log "installing /Library/LaunchDaemons/$plist"
    install -o root -g wheel -m 644 "$SRC/$plist" "/Library/LaunchDaemons/$plist"
done

# --- pf anchor --------------------------------------------------------------------
log "rendering /etc/pf.anchors/dev.warren (subnet $SUBNET, gateway $GATEWAY)"
install -d -o root -g wheel -m 755 /etc/pf.anchors
sed -e "s|@SUBNET@|$SUBNET|g" -e "s|@GATEWAY@|$GATEWAY|g" "$SRC/pf.anchor.template" >/etc/pf.anchors/dev.warren.tmp
if grep -q -e '@SUBNET@' -e '@GATEWAY@' /etc/pf.anchors/dev.warren.tmp; then
    die "placeholders left in the rendered pf anchor"
fi
pfctl -n -a dev.warren -f /etc/pf.anchors/dev.warren.tmp >/dev/null 2>&1 || {
    pfctl -n -a dev.warren -f /etc/pf.anchors/dev.warren.tmp || true
    die "the rendered pf anchor does not parse"
}
mv -f /etc/pf.anchors/dev.warren.tmp /etc/pf.anchors/dev.warren
chown root:wheel /etc/pf.anchors/dev.warren
chmod 644 /etc/pf.anchors/dev.warren

PF_CONF=/etc/pf.conf
[ -f "$PF_CONF" ] || die "$PF_CONF does not exist"
if [ -n "$(tail -c 1 "$PF_CONF")" ]; then
    echo >>"$PF_CONF"   # the stock file ends with a newline; make sure before appending
fi
if grep -qxF 'anchor "dev.warren"' "$PF_CONF"; then
    log "$PF_CONF already references anchor dev.warren"
else
    log "adding anchor dev.warren to $PF_CONF"
    printf '%s\n' 'anchor "dev.warren"' >>"$PF_CONF"
fi
if ! grep -qxF 'load anchor "dev.warren" from "/etc/pf.anchors/dev.warren"' "$PF_CONF"; then
    printf '%s\n' 'load anchor "dev.warren" from "/etc/pf.anchors/dev.warren"' >>"$PF_CONF"
fi
pfctl -n -f "$PF_CONF" >/dev/null 2>&1 || { pfctl -n -f "$PF_CONF" || true; die "$PF_CONF does not parse after the change"; }

# -E enables pf and bumps its reference count; it succeeds when pf is already
# enabled (unlike -e). pfctl always prints an ALTQ notice on macOS; ignore it.
log "loading $PF_CONF and enabling pf"
pfctl -E -f "$PF_CONF" 2>&1 | grep -v -i 'ALTQ' | sed 's/^/  pfctl: /' >&2 || true
pfctl -s info 2>/dev/null | grep -q '^Status: Enabled' || die "pf is not enabled after pfctl -E"
log "active dev.warren rules:"
pfctl -a dev.warren -s rules 2>/dev/null | sed 's/^/  /' >&2
[ -n "$(pfctl -a dev.warren -s rules 2>/dev/null)" ] || die "anchor dev.warren loaded no rules"

# --- lume's home ------------------------------------------------------------------
log "preparing $GUEST_HOME/.warren and $GUEST_HOME/Library/Logs"
install -d -o "$GUEST_USER" -g "$GUEST_GROUP" -m 755 "$GUEST_HOME/.warren"
install -d -o "$GUEST_USER" -g "$GUEST_GROUP" -m 755 "$GUEST_HOME/Library/Logs"
touch "$GUEST_HOME/Library/Logs/warren-gateway.log"
chown "$GUEST_USER:$GUEST_GROUP" "$GUEST_HOME/Library/Logs/warren-gateway.log"
chmod 644 "$GUEST_HOME/Library/Logs/warren-gateway.log"

if [ -n "$SSH_KEY" ]; then
    case "$SSH_KEY" in
        ssh-ed25519\ *|ssh-rsa\ *|ecdsa-sha2-*|sk-*) ;;
        *) die "--ssh-key does not look like an OpenSSH public key: '$SSH_KEY'" ;;
    esac
    install -d -o "$GUEST_USER" -g "$GUEST_GROUP" -m 700 "$GUEST_HOME/.ssh"
    AUTH="$GUEST_HOME/.ssh/authorized_keys"
    touch "$AUTH"
    if grep -qxF "$SSH_KEY" "$AUTH"; then
        log "ssh key already in $AUTH"
    else
        log "adding ssh key to $AUTH"
        printf '%s\n' "$SSH_KEY" >>"$AUTH"
    fi
    chown "$GUEST_USER:$GUEST_GROUP" "$AUTH"
    chmod 600 "$AUTH"
fi

# --- power and updates ------------------------------------------------------------
log "disabling sleep and automatic software updates"
pmset -a sleep 0 displaysleep 0 disksleep 0 2>&1 | sed 's/^/  pmset: /' >&2 || log "warning: pmset failed (continuing)"
softwareupdate --schedule off 2>&1 | sed 's/^/  softwareupdate: /' >&2 || log "warning: softwareupdate --schedule off failed (continuing)"
# Recent macOS reports "turned on" from --schedule off; the preference keys are what the
# update daemon actually reads, so set them as well.
for key in AutomaticCheckEnabled AutomaticDownload AutomaticallyInstallMacOSUpdates ConfigDataInstall CriticalUpdateInstall; do
    defaults write /Library/Preferences/com.apple.SoftwareUpdate "$key" -bool false 2>/dev/null || true
done
defaults write /Library/Preferences/com.apple.commerce AutoUpdate -bool false 2>/dev/null || true

# --- launchd ----------------------------------------------------------------------
bootstrap() {
    local label=$1 plist=$2 attempt
    if launchctl print "system/$label" >/dev/null 2>&1; then
        log "$label is already loaded; booting it out first"
        launchctl bootout "system/$label" 2>/dev/null || true
        sleep 1
    fi
    for attempt in 1 2 3 4 5; do
        if launchctl bootstrap system "$plist" 2>/dev/null; then
            log "$label bootstrapped"
            return 0
        fi
        # A bootout can take a moment to finish; a plain reload of an already
        # loaded service also lands here.
        if launchctl print "system/$label" >/dev/null 2>&1 && [ "$attempt" -gt 2 ]; then
            log "$label is loaded (bootstrap reported it was already there)"
            return 0
        fi
        sleep 1
    done
    launchctl bootstrap system "$plist" || die "could not bootstrap $label from $plist"
}
bootstrap dev.warren.pf /Library/LaunchDaemons/dev.warren.pf.plist
bootstrap dev.warren.gateway /Library/LaunchDaemons/dev.warren.gateway.plist

echo
echo "launchctl print system/dev.warren.gateway:"
# Captured first: `launchctl print | head` would die of SIGPIPE under pipefail.
PRINT="$(launchctl print system/dev.warren.gateway 2>&1 || true)"
printf '%s\n' "$PRINT" | sed -n '1,20p' | sed 's/^/  /'
echo
echo "Installed. In the base image no 'share' folder is attached, so the launcher"
echo "waits 300 s, exits 1 and launchd restarts it every 5 s: that is expected."
echo "It settles once warrend starts a clone of this VM with the share attached."
echo "Log inside the guest: $GUEST_HOME/Library/Logs/warren-gateway.log"
