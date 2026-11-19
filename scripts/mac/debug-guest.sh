#!/bin/bash
# Boot a throwaway clone of the base VM with a shared folder attached, the way
# warrend does, and print what the guest sees: the virtiofs mount, the share's
# contents, the gateway launcher's log, the LaunchDaemon state, and whether the
# gateway answers from the host. For diagnosing "the gateway inside the VM did
# not come up".
#
# Usage: scripts/mac/debug-guest.sh [--base-vm NAME] [--gateway-bin PATH] [--ssh-key PATH] [--keep]
set -euo pipefail

BASE_VM=warren-macos-26
GW=""
SSH_KEY="$HOME/.ssh/id_ed25519"
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --base-vm) BASE_VM="${2:-}"; shift ;;
        --gateway-bin) GW="${2:-}"; shift ;;
        --ssh-key) SSH_KEY="${2:-}"; shift ;;
        --keep) KEEP=1 ;;
        -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done
log() { printf '[debug-guest] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$(pwd)/target}"
GW="${GW:-$CARGO_TARGET_DIR/release/warren-gateway}"
[ -x "$GW" ] || die "$GW not found; run cargo build --release -p warren-gateway or pass --gateway-bin"
command -v jq >/dev/null 2>&1 || die "jq is required (brew install jq)"

VM="dbg-$(date +%s | tail -c 5)$RANDOM"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/warren-debug.XXXXXX")"
SHARE="$WORK/share"
mkdir -p "$SHARE"
cp "$GW" "$SHARE/warren-gateway"
chmod 755 "$SHARE/warren-gateway"
cat >"$SHARE/warren.env" <<ENV
export WARREN_HOME='/Users/lume'
export WARREN_INSTANCE_ID='debug00001'
export WARREN_INSTANCE_TOKEN='debug-token'
export WARREN_WORKSPACE_ID='ws_debug'
export WARREN_GATEWAY_PORT='3737'
export WARREN_AGENT_DRIVER='echo'
ENV
log "share prepared at $SHARE"

cleanup() {
    if [ "$KEEP" = 1 ]; then
        log "--keep: leaving $VM running (share $SHARE); remove with: lume stop $VM; lume delete $VM --force"
        return
    fi
    log "stopping and deleting $VM"
    lume stop "$VM" >/dev/null 2>&1 || true
    lume delete "$VM" --force >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT

log "cloning $BASE_VM -> $VM"
lume clone "$BASE_VM" "$VM" >/dev/null
log "starting $VM headless with the share attached read-only"
lume run "$VM" --detach --display none --vnc disabled --shared-dir "$SHARE:ro" >/dev/null

IP=""
for _ in $(seq 1 90); do
    IP="$(lume get "$VM" -f json 2>/dev/null | jq -r '(if type=="array" then .[0] else . end) | .ipAddress // empty')"
    [ -n "$IP" ] && break
    sleep 2
done
[ -n "$IP" ] || die "$VM got no IP address within 180 s"
log "$VM is at $IP; waiting for SSH"
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=3 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -i "$SSH_KEY" "lume@$IP")
for _ in $(seq 1 60); do
    "${SSH[@]}" true 2>/dev/null && break
    sleep 2
done
"${SSH[@]}" true 2>/dev/null || die "cannot ssh to lume@$IP with $SSH_KEY (is the key installed in the base VM?)"

log "giving the launcher 45 s to find the share and start the gateway"
sleep 45

echo
echo "===== guest: macOS version, uptime, user ====="
"${SSH[@]}" 'sw_vers; uptime; id' 2>&1 || true
echo
echo "===== guest: virtiofs mounts ====="
"${SSH[@]}" 'mount | grep -iE "virtiofs|Shared" || echo "(no virtiofs mount)"' 2>&1 || true
echo
echo "===== guest: /Volumes and the share ====="
"${SSH[@]}" 'ls -la /Volumes/ 2>&1; echo ---; ls -la "/Volumes/My Shared Files/" 2>&1; echo ---; ls -la "/Volumes/My Shared Files/share/" 2>&1' || true
echo
echo "===== guest: launcher and gateway log (last 40 lines) ====="
"${SSH[@]}" 'tail -n 40 /Users/lume/Library/Logs/warren-gateway.log 2>&1 || echo "(no log file)"' || true
echo
echo "===== guest: installed files (plists, launcher, pf anchor, pf.conf lines) ====="
"${SSH[@]}" 'ls -la /Library/LaunchDaemons/ 2>&1 | grep -i -E "warren|total" ; echo ---; ls -la /usr/local/libexec/ 2>&1; echo ---; ls -la /etc/pf.anchors/ 2>&1; echo ---; grep -n "dev.warren" /etc/pf.conf 2>&1 || echo "(no dev.warren lines in /etc/pf.conf)"' || true
echo
echo "===== guest: launchd disabled list and loaded services mentioning warren ====="
"${SSH[@]}" 'echo lume | sudo -S -p "" launchctl print-disabled system 2>/dev/null | grep -i warren || echo "(not in the disabled list)"; echo ---; echo lume | sudo -S -p "" launchctl list 2>/dev/null | grep -i warren || echo "(no warren service loaded)"' || true
echo
echo "===== guest: bootstrapping dev.warren.gateway by hand (shows the error if the plist is there but refused) ====="
"${SSH[@]}" 'if [ -f /Library/LaunchDaemons/dev.warren.gateway.plist ]; then echo lume | sudo -S -p "" launchctl bootstrap system /Library/LaunchDaemons/dev.warren.gateway.plist 2>&1 && echo "bootstrap ok"; sleep 15; tail -n 20 /Users/lume/Library/Logs/warren-gateway.log 2>&1 || echo "(still no log file)"; echo lume | sudo -S -p "" lsof -nP -iTCP:3737 -sTCP:LISTEN 2>/dev/null || echo "(nothing listens on 3737 after the manual bootstrap)"; else echo "plist missing: the base image lost the installed files"; fi' || true
echo
echo "===== guest: LaunchDaemon state ====="
"${SSH[@]}" 'echo lume | sudo -S -p "" launchctl print system/dev.warren.gateway 2>&1 | head -40' || true
echo
echo "===== guest: processes and listeners ====="
"${SSH[@]}" 'ps aux | grep -E "[w]arren-gateway|[l]auncher" || echo "(no gateway process)"; echo ---; echo lume | sudo -S -p "" lsof -nP -iTCP:3737 -sTCP:LISTEN 2>/dev/null || echo "(nothing listens on 3737)"' || true
echo
echo "===== guest: pf status ====="
"${SSH[@]}" 'echo lume | sudo -S -p "" pfctl -s info 2>/dev/null | head -3; echo lume | sudo -S -p "" pfctl -a dev.warren -s rules 2>/dev/null' || true
echo
echo "===== host: gateway health over the network ====="
curl -s -m 5 "http://$IP:3737/v1/health" && echo || echo "(no answer from $IP:3737)"
echo
echo "===== host: lume get $VM ====="
lume get "$VM" -f json | jq -c '(if type=="array" then .[0] else . end) | {status, ipAddress, sharedDirectories, savedState}'
