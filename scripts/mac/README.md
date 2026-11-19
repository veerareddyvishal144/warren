# macOS sandboxes: scripts for the Mac host

These scripts build the base macOS VM and exercise `mac` instances end to end
on an Apple silicon Mac. The design, Lume facts and acceptance criteria are in
[docs/macos.md](../../docs/macos.md).

## Prerequisites

- An Apple silicon Mac running macOS 14 or newer (Virtualization.framework;
  at most two macOS guests run at once).
- [Lume](https://github.com/trycua/cua) 0.7.2 or newer on `PATH` as `lume`.
- `jq` (`brew install jq`) and `curl`.
- The Rust toolchain (`rustup`), to build `warrend` and `warren-gateway`
  natively for `aarch64-apple-darwin`.
- A base VM created from Apple's IPSW, as described in docs/macos.md
  (`lume create ... --ipsw ...`, user `lume`, password `lume`, SSH enabled).
  Below it is called `base`; it must be stopped.
- An SSH key pair at `~/.ssh/id_ed25519` (`ssh-keygen -t ed25519`). The public
  key is installed in the guest.

## The three commands, in order

### 1. `zsh scripts/mac/boot-test.sh [vm]`

Phase 0 measurement: stops the VM (default `sandbox`), boots it again and
prints `cold boot to ssh: N s (ip ...)`, then the memory of the Lume and
Virtualization helper processes. The VM keeps running; stop it with
`lume stop <vm>`.

### 2. `bash scripts/mac/build-base.sh`

Clones `base` to `warren-macos-26`, boots the clone headless, copies
`scripts/mac/guest/` into it and runs `install.sh` as root, which installs:

- `/usr/local/libexec/warren-gateway-launcher.sh` and the LaunchDaemon
  `dev.warren.gateway` (root; the launcher starts the gateway as `lume` from
  the `share` folder warrend attaches, inside lume's own launchd domain via
  `launchctl asuser` and `sudo`, because Apple's `nohup` fails in the system
  domain; restarted by launchd on exit),
- the pf anchor `dev.warren` (blocks the NAT subnet except the host) and the
  LaunchDaemon `dev.warren.pf` that enables pf at boot,
- your SSH public key for `lume`, no system sleep, no automatic updates.

If the key is not in the guest yet, ssh asks for the guest password once
(default `lume`). The script then stops the VM and prints a summary ending in
the `warrend --runtime none --mac-base-vm warren-macos-26 ...` command. Flags:
`--source`, `--target`, `--user`, `--password`, `--ssh-key`, `--subnet`,
`--gateway`, `--force` (replace an existing target).

In the base image the launcher cannot find the share, so
`launchctl print system/dev.warren.gateway` shows it restarting every few
seconds. That is expected; it settles in clones started by warrend.

### 3. `bash scripts/mac/e2e.sh`

Builds `warrend`, `warren-gateway` and `warren` (`--skip-build` to reuse),
starts `warrend --runtime none --mac-base-vm warren-macos-26 --dev` on port
7337 with data under `./data-mac-e2e`, and walks the product against real VMs:
health (runtime `lume`), signup, create a `warren-macos`/`mac` instance,
health through the edge, exec (`sw_vers`), sleep (`savedState: true` in
`lume get`), wake (the marker process is still alive), fork, VM-to-VM
isolation (`nc` from one guest to the other fails), a third create answers
`409 host_capacity_reached`, delete both and check `lume ls`.

It prints a table of PASS/FAIL per step with the create-to-running, sleep,
wake and fork seconds, exits non-zero on any failure and then shows the last
40 lines of `data-mac-e2e/warrend.log`. Flags: `--base-vm`, `--port`,
`--data-dir`, `--base-domain`, `--keep` (leave the instances and the daemon
running), `--skip-build`, `--clean-leftovers` (delete stale `warren-*` VMs
from an earlier run before starting).

Requests go to `http://127.0.0.1:PORT` with a `Host:` header
(`api.warren.localhost:PORT`, `<id>.warren.localhost:PORT`) because macOS's
resolver does not resolve `*.localhost` for `curl`.

### When the guest misbehaves: `bash scripts/mac/debug-guest.sh [--keep]`

Boots a throwaway clone of the base VM with a `share` folder attached the way
warrend does, waits for the launcher, and prints what the guest sees: the
virtiofs mount, the launcher's log, the LaunchDaemon state, the pf rules,
the gateway's health from the host, which launchd domain exec runs in and
whether `nohup` works there, and whether background processes started
through exec survive (plain `nohup &`, own session, own process group).
`--keep` leaves the clone running for manual probing over `ssh lume@<ip>`.
