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
  Below it is called `base`; it must be stopped. The stock password is only
  used to log in during the build; `build-base.sh` replaces it in the image.
- An SSH key pair at `~/.ssh/id_ed25519` (`ssh-keygen -t ed25519`). The public
  key is installed in the guest, and it is the only way in afterwards.
- `openssl` (macOS ships LibreSSL's) to generate the guest password.
- For `--with-agents`: internet access from the VM (Lume's NAT), to fetch
  Node from nodejs.org and the CLIs from the npm registry.

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
- your SSH public key for `lume`, no system sleep, no automatic updates,
- a new password for `lume`: random (`openssl rand -base64 18`) unless
  `--new-password` is given, set with `dscl . -passwd` and verified with
  `dscl . -authonly`; autologin is re-armed with it (`sysadminctl -autologin
  set`). The script writes it to `$WARREN_MAC_DIR/<target>.password`
  (default `~/.warren/mac/warren-macos-26.password`, mode 600) before the
  install starts, uses it for the final `sudo shutdown`, and prints where it
  is. It is the `sudo` password inside every clone; `lume ssh`, `lume
  shutdown` and `lume restart`, which assume `lume`/`lume`, no longer work
  on the image,
- `/etc/ssh/sshd_config.d/100-warren.conf`: `PasswordAuthentication no`,
  `KbdInteractiveAuthentication no`, `PubkeyAuthentication yes`, checked with
  `sshd -T` inside the guest (the `Include` line is added to `sshd_config`
  if missing). The script then confirms from the host that the guest offers
  `publickey` only and that `sudo` takes the new password,
- with `--with-agents`: Node LTS (the newest LTS line listed in
  `https://nodejs.org/dist/index.tab`, or `--node-version vX.Y.Z`) from the
  official `darwin-arm64` tarball, its SHA256 checked against the release's
  `SHASUMS256.txt`, unpacked into `/usr/local`; then `npm install -g
  @anthropic-ai/claude-code @openai/codex opencode-ai` and `claude
  --version`, `codex --version`, `opencode --version` run as `lume` (with a
  scratch `HOME`, so nothing lands in `/Users/lume`). About 1 GB and several
  minutes through the NAT.

If the key is not in the guest yet, ssh asks for the guest password once
(`--password`, default `lume`). The script then stops the VM and prints a
summary (guest user, password file, pf anchor, daemons, agents) ending in
the `warrend --runtime none --mac-base-vm warren-macos-26 ...` command. Flags:
`--source`, `--target`, `--user`, `--password` (the current one),
`--new-password`, `--ssh-key`, `--subnet`, `--gateway`, `--with-agents`,
`--node-version`, `--force` (replace an existing target).

In the base image the launcher cannot find the share, so
`launchctl print system/dev.warren.gateway` shows it restarting every few
seconds. That is expected; it settles in clones started by warrend.

### 3. `bash scripts/mac/e2e.sh`

Builds `warrend`, `warren-gateway` and `warren` (`--skip-build` to reuse),
starts `warrend --runtime none --mac-base-vm warren-macos-26 --dev` on port
7337 with data under `./data-mac-e2e`, and walks the product against real VMs:
health (runtime `lume`), signup, create a `warren-macos`/`mac` instance,
health through the edge, exec (`sw_vers`), a `nohup` marker process, sleep
(`savedState: true` in `lume get`), wake (the marker is still alive with the
same pid), a `warrend` restart (the VM keeps running, the reconciler adopts
it, the marker pid is unchanged, no second VM), fork, VM-to-VM isolation
(`nc` from one guest to the other fails), a third create answers
`409 host_capacity_reached`, delete both and check `lume ls`.

It prints a table of PASS/FAIL per step with the create-to-running, sleep,
wake, restart, fork and delete seconds, exits non-zero on any failure and then shows the last
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
the sshd drop-in and `sshd -T`'s authentication settings, which agent CLIs
are installed, the gateway's health from the host, which launchd domain exec
runs in and whether `nohup` works there, and whether background processes
started through exec survive (plain `nohup &`, own session, own process
group). The root-only probes use `sudo` inside the guest with the password
`build-base.sh` stored in `$WARREN_MAC_DIR/<base vm>.password` (`--password`
or `--password-file` to override; an image built before the rotation still
answers to `lume`, which the script falls back to with a warning). `--keep`
leaves the clone running for manual probing over `ssh -i ~/.ssh/id_ed25519
lume@<ip>`.
