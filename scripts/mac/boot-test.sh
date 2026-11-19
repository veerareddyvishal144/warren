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

lume stop "$VM" 2>/dev/null
sleep 3
START=$(date +%s)
lume run "$VM" || exit 1

IP=""
while [ -z "$IP" ]; do
  IP=$(lume get "$VM" 2>/dev/null | awk '/^ip:/ && $2 ~ /^[0-9]/ {print $2}')
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
