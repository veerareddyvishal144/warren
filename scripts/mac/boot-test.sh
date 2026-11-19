#!/bin/sh
# Phase 0 measurement for macOS sandboxes: cold boot to SSH and VM memory.
#
# Stops the VM, starts it again, waits until SSH answers with the given key,
# prints the elapsed seconds, then lists the memory of the lume processes and
# of Apple's Virtualization helper (where the guest's memory actually lives).
#
# Usage: scripts/mac/boot-test.sh [vm-name] [ssh-user] [identity-file]
# Defaults: sandbox lume ~/.ssh/id_ed25519
set -u
VM=${1:-sandbox}
SSH_USER=${2:-lume}
KEY=${3:-$HOME/.ssh/id_ed25519}

# `lume run` keeps the virtual machine inside its own process and does not
# return until the VM stops, so it runs in the background and survives this
# script (and the terminal) ending. Its output goes to a log file.
LOG=$HOME/.lume/$VM-run.log
lume stop "$VM" 2>/dev/null
sleep 3
START=$(date +%s)
nohup lume run "$VM" >"$LOG" 2>&1 &
RUN_PID=$!
echo "lume run started in the background (pid $RUN_PID, log $LOG)"

# The first non-loopback IPv4 address in `lume get` (its output carries
# formatting when piped, so match the address itself rather than the label).
IP=""
while [ -z "$IP" ]; do
  IP=$(lume get "$VM" 2>&1 | grep -oE '([0-9]{1,3}\.){3}[0-9]{1,3}' | grep -v '^127\.' | head -1)
  [ -z "$IP" ] && sleep 1
done
until ssh -o BatchMode=yes -o ConnectTimeout=2 -o StrictHostKeyChecking=accept-new \
    -i "$KEY" "$SSH_USER@$IP" true 2>/dev/null; do
  sleep 1
done
END=$(date +%s)
echo "cold boot to ssh: $((END - START)) s (ip $IP)"

sleep 20
echo "memory per process:"
ps -eo rss,command | grep -iE '[l]ume|[V]irtualization' | awk '{printf "  %d MB  %s\n", $1/1024, $2}'
echo "the VM keeps running; stop it with: lume stop $VM"
