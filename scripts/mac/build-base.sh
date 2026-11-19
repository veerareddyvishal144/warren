#!/usr/bin/env bash
# Builds the Warren macOS base VM on an Apple silicon Mac: clones a stock Lume
# macOS VM, boots the clone, installs the guest pieces from scripts/mac/guest
# (gateway LaunchDaemon and launcher, pf isolation anchor, SSH key, no sleep,
# no auto-updates) and stops it. warrend then clones this VM for every `mac`
# instance (docs/macos.md).
#
# Usage: bash scripts/mac/build-base.sh [options]
#   --source NAME      stock Lume VM to clone (default base); must be stopped
#   --target NAME      name of the base VM to create (default warren-macos-26)
#   --user USER        guest user (default lume)
#   --password PASS    guest user's password, fed to sudo -S inside the guest (default lume)
#   --ssh-key FILE     private key; FILE.pub is installed in the guest (default ~/.ssh/id_ed25519)
#   --subnet CIDR      Lume NAT subnet the pf anchor blocks (default 192.168.64.0/24)
#   --gateway IP       host address the pf anchor allows (default 192.168.64.1)
#   --force            stop and delete an existing target first
#   -h, --help         this text
#
# SSH: the key is tried first (BatchMode). When the guest does not know it yet,
# ssh asks for the guest password once (ControlMaster keeps the session for the
# copy and the install), after which the key is installed for next time.
#
# Needs: lume >= 0.7.2, ssh/scp, and jq (recommended: brew install jq; a sed
# fallback is used without it).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GUEST_DIR="$HERE/guest"

log() { printf '[build-base] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

SOURCE=base TARGET=warren-macos-26 GUEST_USER=lume PASSWORD=lume
SSH_KEY="$HOME/.ssh/id_ed25519" SUBNET=192.168.64.0/24 GATEWAY=192.168.64.1 FORCE=0
while [ $# -gt 0 ]; do
    case "$1" in
        --source) SOURCE="${2:-}"; shift ;;
        --target) TARGET="${2:-}"; shift ;;
        --user) GUEST_USER="${2:-}"; shift ;;
        --password) PASSWORD="${2:-}"; shift ;;
        --ssh-key) SSH_KEY="${2:-}"; shift ;;
        --subnet) SUBNET="${2:-}"; shift ;;
        --gateway) GATEWAY="${2:-}"; shift ;;
        --force) FORCE=1 ;;
        -h|--help) sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1 (try --help)" ;;
    esac
    shift
done
[ -n "$SOURCE" ] && [ -n "$TARGET" ] || die "--source and --target must not be empty"
[ "$SOURCE" != "$TARGET" ] || die "--source and --target are the same VM ($SOURCE)"
[ "$(uname -s)" = Darwin ] || die "this script runs on the Mac host (Lume needs Apple silicon)"
[ "$(uname -m)" = arm64 ] || die "Apple silicon required (uname -m is $(uname -m))"
[ -d "$GUEST_DIR" ] || die "$GUEST_DIR is missing"

# --- tools ------------------------------------------------------------------------
command -v lume >/dev/null 2>&1 || die "lume is not installed or not on PATH (https://github.com/trycua/cua, 0.7.2 or newer)"
HAVE_JQ=1
if ! command -v jq >/dev/null 2>&1; then
    HAVE_JQ=0
    log "warning: jq not found; using a sed fallback to read lume's JSON (brew install jq is recommended)"
fi

# version_ge A B: true when dotted version A >= B (BSD sort has no -V).
version_ge() {
    local IFS=. i a b
    local -a va vb
    # shellcheck disable=SC2206  # split on dots on purpose
    va=($1) vb=($2)
    for i in 0 1 2; do
        a=${va[$i]:-0}; b=${vb[$i]:-0}
        if [ "$a" -gt "$b" ]; then return 0; fi
        if [ "$a" -lt "$b" ]; then return 1; fi
    done
    return 0
}
LUME_VERSION="$(lume --version 2>&1 | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n 1 || true)"
[ -n "$LUME_VERSION" ] || die "could not read the lume version from 'lume --version'"
version_ge "$LUME_VERSION" 0.7.2 || die "lume $LUME_VERSION is too old; 0.7.2 or newer is required"
log "lume $LUME_VERSION"

# lume_field VM KEY: one field of `lume get VM -f json`, empty when unset/null.
lume_field() {
    local vm=$1 key=$2 json
    json="$(lume get "$vm" -f json 2>/dev/null)" || return 1
    if [ "$HAVE_JQ" = 1 ]; then
        # `// empty` would also swallow a JSON false (savedState, sshAvailable).
        printf '%s' "$json" | jq -r --arg k "$key" '(if type=="array" then .[0] else . end) | .[$k] | select(. != null)'
    else
        printf '%s' "$json" | tr -d '\n' | sed -nE "s/.*\"$key\"[[:space:]]*:[[:space:]]*\"?([^\",}]*)\"?.*/\1/p" | sed 's/^null$//'
    fi
}
vm_exists() { lume get "$1" -f json >/dev/null 2>&1; }

# --- key ----------------------------------------------------------------------------
PUBKEY_FILE="$SSH_KEY.pub"
[ -f "$SSH_KEY" ] || die "private key $SSH_KEY not found (create one with: ssh-keygen -t ed25519 -f $SSH_KEY -N '')"
[ -f "$PUBKEY_FILE" ] || die "public key $PUBKEY_FILE not found next to $SSH_KEY"
PUBKEY="$(head -n 1 "$PUBKEY_FILE")"
case "$PUBKEY" in
    ssh-ed25519\ *|ssh-rsa\ *|ecdsa-sha2-*|sk-*) ;;
    *) die "$PUBKEY_FILE does not look like an OpenSSH public key" ;;
esac

# --- source and target VMs ---------------------------------------------------------
vm_exists "$SOURCE" || die "source VM '$SOURCE' does not exist (lume ls)"
SOURCE_STATUS="$(lume_field "$SOURCE" status)"
[ "$SOURCE_STATUS" = stopped ] || die "source VM '$SOURCE' is '$SOURCE_STATUS'; stop it first (lume stop $SOURCE) so the clone is consistent"

if vm_exists "$TARGET"; then
    [ "$FORCE" = 1 ] || die "target VM '$TARGET' already exists; pass --force to replace it (lume stop/delete) or pick another --target"
    log "replacing existing VM $TARGET"
    if [ "$(lume_field "$TARGET" status)" = running ]; then
        lume stop "$TARGET" || log "warning: lume stop $TARGET failed; deleting anyway"
    fi
    lume delete "$TARGET" --force
    vm_exists "$TARGET" && die "could not delete $TARGET"
fi

log "cloning $SOURCE -> $TARGET (APFS clone, instant)"
lume clone "$SOURCE" "$TARGET"
vm_exists "$TARGET" || die "lume clone finished but $TARGET is not listed"

# --- boot -------------------------------------------------------------------------
# `lume run` is the VM; --detach puts it in the background and returns.
log "starting $TARGET headless"
lume run "$TARGET" --detach --display none --vnc disabled

log "waiting up to 180 s for an IP address and SSH"
IP="" deadline=$((SECONDS + 180))
while :; do
    IP="$(lume_field "$TARGET" ipAddress || true)"
    SSH_OK="$(lume_field "$TARGET" sshAvailable || true)"
    if [ -n "$IP" ] && [ "$SSH_OK" = true ]; then break; fi
    if [ "$SECONDS" -ge "$deadline" ]; then
        log "lume get $TARGET -f json:"; lume get "$TARGET" -f json >&2 || true
        die "$TARGET did not report an IP address and SSH within 180 s"
    fi
    sleep 2
done
log "$TARGET is up at $IP"

# --- stage the guest files ------------------------------------------------------------
STAGE="$(mktemp -d "${TMPDIR:-/tmp}/warren-guest.XXXXXX")"
cp "$GUEST_DIR"/warren-gateway-launcher.sh "$GUEST_DIR"/install.sh "$GUEST_DIR"/pf.anchor.template \
   "$GUEST_DIR"/dev.warren.gateway.plist "$GUEST_DIR"/dev.warren.pf.plist "$STAGE/"
printf '%s\n' "$PUBKEY" >"$STAGE/authorized_key.pub"
bash -n "$STAGE/install.sh"
bash -n "$STAGE/warren-gateway-launcher.sh"
if command -v plutil >/dev/null 2>&1; then
    plutil -lint "$STAGE/dev.warren.gateway.plist" "$STAGE/dev.warren.pf.plist" >/dev/null || die "a plist in $GUEST_DIR is invalid"
fi

# --- ssh session ------------------------------------------------------------------
# Clones reuse addresses, so host keys are neither checked nor stored. One
# ControlMaster connection carries the copy and the install.
CTRL="/tmp/warren-build-base-%C"
SSH_OPTS=(-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR
          -o ConnectTimeout=5 -o ControlMaster=auto -o ControlPath="$CTRL" -o ControlPersist=600)
REMOTE="$GUEST_USER@$IP"
cleanup() {
    ssh "${SSH_OPTS[@]}" -O exit "$REMOTE" >/dev/null 2>&1 || true
    if [ -n "${STAGE:-}" ] && [ -d "$STAGE" ]; then rm -rf "$STAGE"; fi
}
trap cleanup EXIT

KEY_WORKED=0
if ssh "${SSH_OPTS[@]}" -o BatchMode=yes -o PasswordAuthentication=no -i "$SSH_KEY" "$REMOTE" true 2>/dev/null; then
    KEY_WORKED=1
    log "ssh with $SSH_KEY works"
else
    log "the guest does not accept $SSH_KEY yet; falling back to the password of $GUEST_USER"
    if [ -t 0 ]; then
        log "ssh will prompt for the password now (default: lume)"
        ssh "${SSH_OPTS[@]}" -o PreferredAuthentications=password,keyboard-interactive "$REMOTE" true \
            || die "password login to $REMOTE failed"
    elif command -v expect >/dev/null 2>&1; then
        log "no terminal; using expect to answer the password prompt"
        # The password is argv[0] of the expect script, the ssh command the rest.
        cat >"$STAGE/login.exp" <<'EOF'
set timeout 30
set pw [lindex $argv 0]
spawn {*}[lrange $argv 1 end]
expect {
    -re "(?i)password" { send -- "$pw\r"; exp_continue }
    eof {}
    timeout { exit 2 }
}
catch wait result
exit [lindex $result 3]
EOF
        if ! expect -f "$STAGE/login.exp" "$PASSWORD" ssh "${SSH_OPTS[@]}" -o PreferredAuthentications=password,keyboard-interactive "$REMOTE" true; then
            die "password login to $REMOTE failed (expect)"
        fi
        rm -f "$STAGE/login.exp"
    else
        die "no terminal to type the guest password and no 'expect' on PATH; run this from a terminal or install the key in the guest first"
    fi
fi
ssh "${SSH_OPTS[@]}" "$REMOTE" true || die "ssh session to $REMOTE is not usable"

# --- copy the guest files ---------------------------------------------------------------

REMOTE_DIR=/tmp/warren-guest
log "copying guest files to $REMOTE:$REMOTE_DIR"
ssh "${SSH_OPTS[@]}" "$REMOTE" "rm -rf $REMOTE_DIR"
scp -q -r "${SSH_OPTS[@]}" "$STAGE" "$REMOTE:$REMOTE_DIR"

# --- install inside the guest ----------------------------------------------------------
# sudo -S reads the password from stdin; -p '' hides the prompt. -k forces the
# password check so a wrong --password fails here instead of later.
log "running install.sh as root inside the guest"
INSTALL_CMD="sudo -S -k -p '' bash $REMOTE_DIR/install.sh --subnet '$SUBNET' --gateway '$GATEWAY' --ssh-key-file $REMOTE_DIR/authorized_key.pub"
printf '%s\n' "$PASSWORD" | ssh "${SSH_OPTS[@]}" "$REMOTE" "$INSTALL_CMD" || die "install.sh failed inside the guest (see the output above)"

if [ "$KEY_WORKED" = 0 ]; then
    if ssh "${SSH_OPTS[@]}" -o ControlMaster=no -o ControlPath=none -o BatchMode=yes -i "$SSH_KEY" "$REMOTE" true 2>/dev/null; then
        log "key-based ssh now works"
    else
        log "warning: key-based ssh still fails after the install; check /Users/$GUEST_USER/.ssh in the guest"
    fi
fi
ssh "${SSH_OPTS[@]}" "$REMOTE" "rm -rf $REMOTE_DIR" || true

# --- stop ---------------------------------------------------------------------------
sleep 5
ssh "${SSH_OPTS[@]}" -O exit "$REMOTE" >/dev/null 2>&1 || true
# A clean shutdown from inside the guest: `lume stop` signals the VM process,
# which powers the machine off after a short grace period, and a power-off
# seconds after writing files can lose them (the first base image came up
# without its LaunchDaemons). sync first, then let macOS shut itself down.
log "shutting $TARGET down cleanly from inside the guest"
printf '%s\n' "$PASSWORD" | ssh "${SSH_OPTS[@]}" "$REMOTE" "sync; sudo -S -k -p '' shutdown -h now" >/dev/null 2>&1 || true
deadline=$((SECONDS + 120))
while [ "$(lume_field "$TARGET" status || true)" != stopped ]; do
    if [ "$SECONDS" -ge "$deadline" ]; then
        log "warning: $TARGET did not shut down by itself within 120 s; asking lume to stop it"
        lume stop "$TARGET" || true
        break
    fi
    sleep 2
done
deadline=$((SECONDS + 60))
while [ "$(lume_field "$TARGET" status || true)" != stopped ]; do
    [ "$SECONDS" -lt "$deadline" ] || die "$TARGET did not stop (lume get $TARGET)"
    sleep 2
done

cat <<SUMMARY

Base VM ready: $TARGET (cloned from $SOURCE, stopped)
  guest user      $GUEST_USER  (ssh key $PUBKEY_FILE installed)
  pf anchor       blocks $SUBNET except $GATEWAY
  LaunchDaemons   dev.warren.gateway (KeepAlive), dev.warren.pf

Next:
  target/release/warrend --runtime none --mac-base-vm $TARGET --dev \\
      --gateway-bin target/release/warren-gateway --data-dir ./data-mac
  or run the end-to-end check:  bash scripts/mac/e2e.sh --base-vm $TARGET
SUMMARY
