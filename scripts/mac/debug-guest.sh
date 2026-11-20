#!/bin/bash
# Boot a throwaway clone of the base VM with a shared folder attached, the way
# warrend does, and print what the guest sees: the virtiofs mount, the share's
# contents, the gateway launcher's log, the LaunchDaemon state, and whether the
# gateway answers from the host, and whether a process started in the
# background through exec survives (nohup, own session, own process group).
# For diagnosing "the gateway inside the VM did not come up" or "background
# processes die".
#
# Usage: scripts/mac/debug-guest.sh [--base-vm NAME] [--gateway-bin PATH] [--ssh-key PATH]
#                                   [--password PASS | --password-file FILE] [--keep]
#
# The root-only probes use sudo inside the guest. The guest password is the one
# build-base.sh generated, read from $WARREN_MAC_DIR/<base vm>.password
# (default ~/.warren/mac) unless --password or --password-file is given. SSH
# itself is by key: the image refuses password logins.
set -euo pipefail

BASE_VM=warren-macos-26
GW=""
SSH_KEY="$HOME/.ssh/id_ed25519"
PASSWORD="" PASSWORD_FILE=""
KEEP=0
while [ $# -gt 0 ]; do
    case "$1" in
        --base-vm) BASE_VM="${2:-}"; shift ;;
        --gateway-bin) GW="${2:-}"; shift ;;
        --ssh-key) SSH_KEY="${2:-}"; shift ;;
        --password) PASSWORD="${2:-}"; shift ;;
        --password-file) PASSWORD_FILE="${2:-}"; shift ;;
        --keep) KEEP=1 ;;
        -h|--help) sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
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

# --- the guest's sudo password --------------------------------------------------------
MAC_DIR="${WARREN_MAC_DIR:-$HOME/.warren/mac}"
if [ -z "$PASSWORD" ]; then
    PASSWORD_FILE="${PASSWORD_FILE:-$MAC_DIR/$BASE_VM.password}"
    if [ -f "$PASSWORD_FILE" ]; then
        PASSWORD="$(head -n 1 "$PASSWORD_FILE")"
        [ -n "$PASSWORD" ] || die "$PASSWORD_FILE is empty"
        log "guest sudo password read from $PASSWORD_FILE"
    else
        log "warning: $PASSWORD_FILE not found (build-base.sh writes it); trying Lume's stock password for an image built before the rotation"
        PASSWORD=lume
    fi
fi

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
        log "--keep: leaving $VM running (share $SHARE); probe it with: ssh -i $SSH_KEY lume@$IP (sudo password: ${PASSWORD_FILE:-given on the command line}); remove with: lume stop $VM; lume delete $VM --force"
        return
    fi
    log "stopping and deleting $VM"
    lume stop "$VM" >/dev/null 2>&1 || true
    lume delete "$VM" --force >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
IP=""
trap cleanup EXIT

log "cloning $BASE_VM -> $VM"
lume clone "$BASE_VM" "$VM" >/dev/null
log "starting $VM headless with the share attached read-only"
lume run "$VM" --detach --display none --vnc disabled --shared-dir "$SHARE:ro" >/dev/null

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

# guest SCRIPT: run SCRIPT with bash inside the guest. `sudo_ CMD...` in the
# script runs CMD as root; the password travels as the second line of bash's
# stdin (bash reads a piped script line by line, so `read` takes it and the
# rest is the script) and never appears on a command line.
guest() {
    {
        echo 'IFS= read -r PW'
        printf '%s\n' "$PASSWORD"
        # shellcheck disable=SC2016  # a function definition for the guest's shell, sent verbatim
        printf '%s\n' 'sudo_() { printf "%s\n" "$PW" | sudo -S -p "" "$@"; }'
        printf '%s\n' "$1"
    } | "${SSH[@]}" bash -s
}
if guest 'sudo_ -k true 2>/dev/null'; then
    log "sudo inside the guest accepts the password"
else
    log "warning: sudo inside the guest refuses the password; the root-only probes below will print nothing"
fi

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
echo "===== guest: installed files (plists, launcher, pf anchor, pf.conf lines, sshd drop-in) ====="
"${SSH[@]}" 'ls -la /Library/LaunchDaemons/ 2>&1 | grep -i -E "warren|total" ; echo ---; ls -la /usr/local/libexec/ 2>&1; echo ---; ls -la /etc/pf.anchors/ 2>&1; echo ---; grep -n "dev.warren" /etc/pf.conf 2>&1 || echo "(no dev.warren lines in /etc/pf.conf)"; echo ---; cat /etc/ssh/sshd_config.d/100-warren.conf 2>&1 || echo "(no sshd drop-in: password logins are still allowed)"' || true
echo
echo "===== guest: agent CLIs (present only in an image built with --with-agents) ====="
# shellcheck disable=SC2016  # expanded by the guest's shell
"${SSH[@]}" 'export PATH=/usr/local/bin:$PATH; for c in node npm claude codex opencode; do printf "%-9s " "$c"; command -v "$c" >/dev/null 2>&1 && "$c" --version 2>&1 | head -n 1 || echo "(not installed)"; done' || true
# A plain `ssh host cmd` runs zsh without the login PATH, so /usr/local/bin is
# added by hand above; exec through the gateway uses `sh -lc` and sees it anyway.
echo
echo "===== guest: launchd disabled list and loaded services mentioning warren ====="
guest 'sudo_ launchctl print-disabled system 2>/dev/null | grep -i warren || echo "(not in the disabled list)"; echo ---; sudo_ launchctl list 2>/dev/null | grep -i warren || echo "(no warren service loaded)"' || true
echo
echo "===== guest: bootstrapping dev.warren.gateway by hand (shows the error if the plist is there but refused) ====="
guest 'if [ -f /Library/LaunchDaemons/dev.warren.gateway.plist ]; then sudo_ launchctl bootstrap system /Library/LaunchDaemons/dev.warren.gateway.plist 2>&1 && echo "bootstrap ok"; sleep 15; tail -n 20 /Users/lume/Library/Logs/warren-gateway.log 2>&1 || echo "(still no log file)"; sudo_ lsof -nP -iTCP:3737 -sTCP:LISTEN 2>/dev/null || echo "(nothing listens on 3737 after the manual bootstrap)"; else echo "plist missing: the base image lost the installed files"; fi' || true
echo
echo "===== guest: LaunchDaemon state ====="
guest 'sudo_ launchctl print system/dev.warren.gateway 2>&1 | head -40' || true
echo
echo "===== guest: processes and listeners ====="
guest 'ps aux | grep -E "[w]arren-gateway|[l]auncher" || echo "(no gateway process)"; echo ---; sudo_ lsof -nP -iTCP:3737 -sTCP:LISTEN 2>/dev/null || echo "(nothing listens on 3737)"' || true
echo
echo "===== guest: pf status ====="
guest 'sudo_ pfctl -s info 2>/dev/null | head -3; sudo_ pfctl -a dev.warren -s rules 2>/dev/null' || true
echo
echo "===== guest: effective sshd authentication settings ====="
guest 'sudo_ /usr/sbin/sshd -T 2>/dev/null | grep -iE "^(passwordauthentication|kbdinteractiveauthentication|pubkeyauthentication) " || echo "(sshd -T printed nothing)"' || true
echo
echo "===== host: gateway health over the network ====="
curl -s -m 5 "http://$IP:3737/v1/health" && echo || echo "(no answer from $IP:3737)"
echo
echo "===== host -> gateway: does a background process survive exec? ====="
gw_exec() {
    curl -s -m 60 -X POST "http://$IP:3737/v1/exec" -H 'x-warren-instance-token: debug-token' \
        -H 'content-type: application/json' \
        --data-binary "$(jq -cn --arg c "$1" '{command: $c, timeout_seconds: 30}')"
    echo
}
if curl -s -m 5 -o /dev/null "http://$IP:3737/v1/health"; then
    echo "--- a) nohup ... & (what e2e.sh does)"
    gw_exec 'nohup sleep 100000 </dev/null >/dev/null 2>&1 & echo started a'
    echo "--- b) perl setsid + exec (own session)"
    gw_exec 'perl -e "use POSIX; POSIX::setsid(); exec @ARGV" sleep 100001 </dev/null >/dev/null 2>&1 & echo started b'
    echo "--- c) nohup via a nested sh -c"
    gw_exec 'nohup sh -c "exec sleep 100002" </dev/null >/dev/null 2>&1 & echo started c'
    echo "--- d) which launchd domain exec runs in, and whether nohup itself works there"
    gw_exec 'echo "domain: $(launchctl managername 2>&1), uid $(id -u)"; nohup true; echo "nohup rc=$? (127 with \"can'\''t detach from console\" means the system domain; see the launcher)"'
    sleep 3
    echo "--- what is left running (pid ppid pgid sess command)"
    gw_exec 'ps -axo pid,ppid,pgid,sess,user,command | grep "[s]leep 10000" || echo "(no sleep processes)"'
    echo "--- pgrep variants"
    gw_exec 'echo "pgrep -f [s]leep 100000:"; pgrep -f "[s]leep 100000"; echo "rc=$?"; echo "pgrep -lf sleep:"; pgrep -lf sleep; echo "rc=$?"; echo "pgrep -x sleep:"; pgrep -x sleep; echo "rc=$?"'
    echo "--- gateway log tail inside the guest"
    "${SSH[@]}" 'tail -n 15 /Users/lume/Library/Logs/warren-gateway.log 2>&1' || true
    gw_exec 'pkill -f "sleep 10000" 2>/dev/null; echo cleaned'
else
    echo "(gateway not reachable; skipping)"
fi
echo
echo "===== host: lume get $VM ====="
lume get "$VM" -f json | jq -c '(if type=="array" then .[0] else . end) | {status, ipAddress, sharedDirectories, savedState}'
