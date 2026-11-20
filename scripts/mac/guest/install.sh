#!/bin/bash
# Installs the Warren guest pieces INSIDE a macOS VM. Run as root (sudo) from
# the directory scripts/mac/build-base.sh copied here (default /tmp/warren-guest).
#
# Usage: sudo bash install.sh --subnet CIDR --gateway IP [options]
#   --ssh-key "ssh-ed25519 AAAA..."   public key to install for the guest user
#   --ssh-key-file FILE               the same, read from the first line of FILE
#   --new-password PASS               set the guest user's password to PASS (verified with dscl -authonly)
#   --new-password-file FILE          the same, read from the first line of FILE (the file is deleted)
#   --with-agents                     install Node LTS under /usr/local and the claude, codex and opencode CLIs
#   --node-version vX.Y.Z             Node version for --with-agents (default: the latest LTS at nodejs.org)
#
# What it does (every step is idempotent):
#   - /usr/local/libexec/warren-gateway-launcher.sh and the two LaunchDaemons
#     dev.warren.gateway (root; the launcher starts the gateway as lume inside
#     lume's own launchd domain, see the launcher) and dev.warren.pf (enables pf at boot)
#   - /etc/pf.anchors/dev.warren rendered from pf.anchor.template, referenced from /etc/pf.conf, loaded now
#   - /Users/lume/.warren and /Users/lume/Library/Logs owned by lume
#   - the given public key appended to /Users/lume/.ssh/authorized_keys
#   - with --new-password: the guest user's password is replaced (Lume's VMs come with
#     lume/lume) and autologin, when it was on, is re-armed with the new password
#   - when a key is installed: /etc/ssh/sshd_config.d/100-warren.conf turns password
#     and keyboard-interactive SSH authentication off (keys only), checked with sshd -T
#   - with --with-agents: Node LTS from the official arm64 tarball (SHA256 checked against
#     SHASUMS256.txt) into /usr/local, then npm -g @anthropic-ai/claude-code @openai/codex
#     opencode-ai, each checked with --version as the guest user; needs internet (NAT)
#   - no system sleep (pmset) and no automatic software updates
#   - both daemons bootstrapped into the system domain
set -euo pipefail

log() { printf '[install] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

SRC="$(cd "$(dirname "$0")" && pwd)"
SUBNET="" GATEWAY="" SSH_KEY="" SSH_KEY_FILE="" NEW_PASSWORD="" NEW_PASSWORD_FILE=""
WITH_AGENTS=0 NODE_VERSION=""
while [ $# -gt 0 ]; do
    case "$1" in
        --subnet) SUBNET="${2:-}"; shift ;;
        --gateway) GATEWAY="${2:-}"; shift ;;
        --ssh-key) SSH_KEY="${2:-}"; shift ;;
        --ssh-key-file) SSH_KEY_FILE="${2:-}"; shift ;;
        --new-password) NEW_PASSWORD="${2:-}"; shift ;;
        --new-password-file) NEW_PASSWORD_FILE="${2:-}"; shift ;;
        --with-agents) WITH_AGENTS=1 ;;
        --node-version) NODE_VERSION="${2:-}"; shift ;;
        -h|--help) sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1 (try --help)" ;;
    esac
    shift
done
[ "$(id -u)" = 0 ] || die "run as root: sudo bash $0 ..."
[ "$(uname -s)" = Darwin ] || die "this installer only runs inside a macOS guest"
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
if [ -n "$NEW_PASSWORD_FILE" ]; then
    [ -f "$NEW_PASSWORD_FILE" ] || die "--new-password-file $NEW_PASSWORD_FILE does not exist"
    NEW_PASSWORD="$(head -n 1 "$NEW_PASSWORD_FILE")"
    rm -f "$NEW_PASSWORD_FILE"
    [ -n "$NEW_PASSWORD" ] || die "$NEW_PASSWORD_FILE is empty"
fi
if [ -n "$NODE_VERSION" ]; then
    case "$NODE_VERSION" in
        v[0-9]*.[0-9]*.[0-9]*) ;;
        [0-9]*.[0-9]*.[0-9]*) NODE_VERSION="v$NODE_VERSION" ;;
        *) die "--node-version must look like v24.11.0, got '$NODE_VERSION'" ;;
    esac
fi

GUEST_USER=lume
GUEST_GROUP=staff
GUEST_HOME=/Users/$GUEST_USER
id "$GUEST_USER" >/dev/null 2>&1 || die "user $GUEST_USER does not exist in this guest"
# The launcher drops from root to the guest user with `sudo -n`; root needs no
# password for that with the stock sudoers (root ALL=(ALL) ALL), but check.
[ -x /usr/bin/sudo ] || die "/usr/bin/sudo is missing; the launcher needs it to drop privileges"
sudo -n -u "$GUEST_USER" -H true 2>/dev/null || die "root cannot run commands as $GUEST_USER through sudo -n; the launcher relies on that"

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

AUTH="$GUEST_HOME/.ssh/authorized_keys"
if [ -n "$SSH_KEY" ]; then
    case "$SSH_KEY" in
        ssh-ed25519\ *|ssh-rsa\ *|ecdsa-sha2-*|sk-*) ;;
        *) die "--ssh-key does not look like an OpenSSH public key: '$SSH_KEY'" ;;
    esac
    install -d -o "$GUEST_USER" -g "$GUEST_GROUP" -m 700 "$GUEST_HOME/.ssh"
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

# --- guest password -----------------------------------------------------------------
# Lume's unattended install leaves the guest user with the password `lume`.
# Every clone of this image inherits what is set here. dscl as root replaces the
# password without knowing the old one; the login keychain keeps the old password
# (unused in a headless guest). Autologin stores the password in /etc/kcpassword,
# so when it was on it is re-armed through sysadminctl, or the next boot would
# stop at the login window and the gateway would wait for a user session.
if [ -n "$NEW_PASSWORD" ]; then
    log "setting the password of $GUEST_USER"
    AUTOLOGIN_USER="$(defaults read /Library/Preferences/com.apple.loginwindow autoLoginUser 2>/dev/null || true)"
    if ! dscl . -passwd "$GUEST_HOME" "$NEW_PASSWORD" 2>&1 | sed 's/^/  dscl: /' >&2; then
        log "dscl -passwd failed; trying sysadminctl -resetPasswordFor"
        sysadminctl -resetPasswordFor "$GUEST_USER" -newPassword "$NEW_PASSWORD" 2>&1 | sed 's/^/  sysadminctl: /' >&2 || true
    fi
    dscl . -authonly "$GUEST_USER" "$NEW_PASSWORD" >/dev/null 2>&1 \
        || die "the new password of $GUEST_USER does not authenticate (dscl -authonly); the old password is probably still in place"
    log "new password verified with dscl -authonly"
    if [ "$AUTOLOGIN_USER" = "$GUEST_USER" ]; then
        if sysadminctl -autologin set -userName "$GUEST_USER" -password "$NEW_PASSWORD" 2>&1 | sed 's/^/  sysadminctl: /' >&2; then
            log "autologin for $GUEST_USER re-armed with the new password"
        else
            log "warning: could not re-arm autologin for $GUEST_USER; the console will stop at the login window"
        fi
    fi
fi

# --- sshd: keys only -----------------------------------------------------------------
# Only when a key is in place: with password authentication off and no key the
# guest would be reachable only through its console. macOS starts sshd per
# connection from launchd, so the new configuration applies to the next login
# without a restart; the session running this installer is unaffected.
SSHD_CONF=/etc/ssh/sshd_config
SSHD_DROPIN=/etc/ssh/sshd_config.d/100-warren.conf
if [ -s "$AUTH" ]; then
    log "installing $SSHD_DROPIN (password and keyboard-interactive authentication off)"
    install -d -o root -g wheel -m 755 /etc/ssh/sshd_config.d
    cat >"$SSHD_DROPIN.tmp" <<'EOF'
# Warren guest (scripts/mac/guest/install.sh): SSH with keys only. The guest
# user's password is used for sudo, never for logging in from the network.
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
EOF
    chown root:wheel "$SSHD_DROPIN.tmp"
    chmod 644 "$SSHD_DROPIN.tmp"
    mv -f "$SSHD_DROPIN.tmp" "$SSHD_DROPIN"
    [ -f "$SSHD_CONF" ] || die "$SSHD_CONF does not exist"
    # sshd takes the first value it sees for a keyword, so the Include must come
    # before any setting in sshd_config itself. macOS 13 and newer ship it at the top.
    if grep -qE '^[[:space:]]*Include[[:space:]]+/etc/ssh/sshd_config\.d/\*' "$SSHD_CONF"; then
        log "$SSHD_CONF already includes /etc/ssh/sshd_config.d/*"
    else
        log "prepending 'Include /etc/ssh/sshd_config.d/*' to $SSHD_CONF"
        { printf '%s\n' 'Include /etc/ssh/sshd_config.d/*'; cat "$SSHD_CONF"; } >"$SSHD_CONF.tmp"
        chown root:wheel "$SSHD_CONF.tmp"
        chmod 644 "$SSHD_CONF.tmp"
        mv -f "$SSHD_CONF.tmp" "$SSHD_CONF"
    fi
    # The effective configuration, as the next sshd will read it.
    EFFECTIVE="$(/usr/sbin/sshd -T 2>/dev/null || true)"
    if [ -z "$EFFECTIVE" ]; then
        log "warning: sshd -T printed nothing; could not verify the effective sshd configuration"
    else
        for want in 'passwordauthentication no' 'kbdinteractiveauthentication no' 'pubkeyauthentication yes'; do
            printf '%s\n' "$EFFECTIVE" | grep -qix "$want" \
                || die "sshd -T does not report '$want' after installing $SSHD_DROPIN; check $SSHD_CONF"
        done
        log "sshd -T confirms: password and keyboard-interactive authentication off, public keys on"
    fi
else
    log "warning: no ssh key installed for $GUEST_USER; leaving sshd's password authentication on so the guest stays reachable"
fi

# --- agents: Node LTS and the CLIs -----------------------------------------------------
# A bounded run of a command: perl's alarm ends it (SIGALRM) after N seconds.
# macOS has no timeout(1); without perl the command simply runs unbounded.
with_timeout() {
    local secs=$1; shift
    if command -v perl >/dev/null 2>&1; then
        perl -e 'alarm shift @ARGV; exec @ARGV or die "exec: $!"' "$secs" "$@"
    else
        "$@"
    fi
}

if [ "$WITH_AGENTS" = 1 ]; then
    [ "$(uname -m)" = arm64 ] || die "--with-agents installs the darwin-arm64 Node build; this guest is $(uname -m)"
    NODE_DIST=https://nodejs.org/dist
    curl -fsS -m 20 -o /dev/null "$NODE_DIST/index.tab" \
        || die "cannot reach $NODE_DIST from the guest; --with-agents needs internet access during the build"
    if [ -z "$NODE_VERSION" ]; then
        # index.tab: newest first, tab-separated; column 10 is the LTS codename
        # or "-". awk reads the whole file (about 100 KB): an early `exit` would
        # close the pipe under curl, which then fails and, with pipefail, so
        # would this assignment.
        NODE_VERSION="$(curl -fsSL --retry 3 --retry-delay 2 "$NODE_DIST/index.tab" \
            | awk -F'\t' 'NR > 1 && $10 != "-" && !found { print $1; found = 1 }')"
        [ -n "$NODE_VERSION" ] || die "could not find the latest Node LTS in $NODE_DIST/index.tab"
        log "latest Node LTS is $NODE_VERSION"
    fi
    NODE_TARBALL="node-$NODE_VERSION-darwin-arm64.tar.gz"
    NODE_WORK="$(mktemp -d /tmp/warren-node.XXXXXX)"
    log "downloading $NODE_DIST/$NODE_VERSION/$NODE_TARBALL"
    curl -fsSL --retry 3 --retry-delay 2 -o "$NODE_WORK/$NODE_TARBALL" "$NODE_DIST/$NODE_VERSION/$NODE_TARBALL" \
        || die "download of $NODE_TARBALL failed"
    curl -fsSL --retry 3 --retry-delay 2 -o "$NODE_WORK/SHASUMS256.txt" "$NODE_DIST/$NODE_VERSION/SHASUMS256.txt" \
        || die "download of SHASUMS256.txt for $NODE_VERSION failed"
    grep -q " $NODE_TARBALL\$" "$NODE_WORK/SHASUMS256.txt" || die "SHASUMS256.txt of $NODE_VERSION has no entry for $NODE_TARBALL"
    (cd "$NODE_WORK" && grep " $NODE_TARBALL\$" SHASUMS256.txt | shasum -a 256 -c - >/dev/null) \
        || die "SHA256 of $NODE_TARBALL does not match SHASUMS256.txt"
    log "SHA256 of $NODE_TARBALL verified"
    if [ -x /usr/local/bin/node ]; then
        log "replacing the existing /usr/local/bin/node ($(/usr/local/bin/node --version 2>/dev/null || echo unknown))"
    fi
    for d in /usr/local /usr/local/bin /usr/local/lib /usr/local/include /usr/local/share; do
        install -d -o root -g wheel -m 755 "$d"
    done
    # bin/, include/, lib/ and share/ of the tarball land in /usr/local; the
    # three files at its top level would litter /usr/local itself.
    tar -xzf "$NODE_WORK/$NODE_TARBALL" -C /usr/local --strip-components 1 --no-same-owner \
        --exclude "node-$NODE_VERSION-darwin-arm64/README.md" \
        --exclude "node-$NODE_VERSION-darwin-arm64/LICENSE" \
        --exclude "node-$NODE_VERSION-darwin-arm64/CHANGELOG.md" \
        || die "could not extract $NODE_TARBALL into /usr/local"
    rm -rf "$NODE_WORK"
    export PATH="/usr/local/bin:$PATH"
    GOT="$(/usr/local/bin/node --version 2>&1 || true)"
    [ "$GOT" = "$NODE_VERSION" ] || die "/usr/local/bin/node --version printed '$GOT', expected $NODE_VERSION"
    log "node $GOT, npm $(/usr/local/bin/npm --version 2>/dev/null || echo '?') installed in /usr/local"

    # Global packages go to /usr/local/lib/node_modules with their commands in
    # /usr/local/bin, runnable by every user. The cache is scratch.
    NPM_CACHE="$(mktemp -d /tmp/warren-npm-cache.XXXXXX)"
    log "npm install -g @anthropic-ai/claude-code @openai/codex opencode-ai (several hundred MB; this takes a few minutes)"
    with_timeout 1800 /usr/local/bin/npm install -g --no-audit --no-fund --cache "$NPM_CACHE" \
        @anthropic-ai/claude-code @openai/codex opencode-ai 2>&1 | sed 's/^/  npm: /' >&2 \
        || die "npm install -g of the agent CLIs failed"
    rm -rf "$NPM_CACHE"
    # claude-code and opencode-ai pick their arm64 binary in a postinstall hook
    # (install.cjs, postinstall.mjs). npm 11 still runs those for global installs
    # but warns they are not in its allow list; run them by hand when a command
    # is missing or does not start, in case a newer npm skips them.
    for pair in "claude:@anthropic-ai/claude-code:install.cjs" "opencode:opencode-ai:postinstall.mjs"; do
        cmd="${pair%%:*}"; rest="${pair#*:}"; pkg="${rest%%:*}"; hook="${rest#*:}"
        if ! with_timeout 120 "/usr/local/bin/$cmd" --version >/dev/null 2>&1; then
            log "$cmd does not run yet; running its postinstall hook $hook"
            (cd "/usr/local/lib/node_modules/$pkg" && /usr/local/bin/node "$hook") 2>&1 | sed "s/^/  $pkg: /" >&2 || true
        fi
    done
    # Each CLI as the guest user, with a scratch HOME so the version checks
    # leave no state in /Users/lume (claude and opencode write to .cache,
    # .config and .local on their first run).
    SCRATCH_HOME="$(mktemp -d /tmp/warren-agent-check.XXXXXX)"
    chown "$GUEST_USER:$GUEST_GROUP" "$SCRATCH_HOME"
    for cmd in claude codex opencode; do
        [ -x "/usr/local/bin/$cmd" ] || die "/usr/local/bin/$cmd is missing after npm install -g"
        if ! out="$(with_timeout 120 sudo -n -u "$GUEST_USER" -- /usr/bin/env HOME="$SCRATCH_HOME" \
                PATH=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin "/usr/local/bin/$cmd" --version 2>&1)"; then
            printf '%s\n' "$out" | sed "s/^/  $cmd: /" >&2
            die "$cmd --version failed as $GUEST_USER"
        fi
        log "$cmd --version as $GUEST_USER: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-120)"
    done
    rm -rf "$SCRATCH_HOME" /var/root/.npm
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
launchctl enable system/dev.warren.pf 2>/dev/null || true
launchctl enable system/dev.warren.gateway 2>/dev/null || true
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
[ -n "$NEW_PASSWORD" ] && echo "Password of $GUEST_USER: replaced (sudo inside the guest needs the new one)."
[ -f "$SSHD_DROPIN" ] && echo "SSH: keys only ($SSHD_DROPIN); password logins are refused from now on."
[ "$WITH_AGENTS" = 1 ] && echo "Agents: node $NODE_VERSION, claude, codex and opencode in /usr/local/bin."

# Everything above must be on disk before the host powers the VM off.
sync
