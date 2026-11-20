# macOS sandboxes

Status: Phase 1 done on 2026-11-19. The `mac` instance type is implemented,
unit-tested against a mock of Lume's API, and `scripts/mac/e2e.sh` passes
end to end, twice in a row, on a MacBook Pro (Apple silicon, Lume 0.7.2,
macOS 26.6.2 guests): create, health, exec, a background process surviving
sleep and wake, adoption of the running VM across a daemon restart, fork,
guest-to-guest isolation, `409 host_capacity_reached` and delete. Measured
numbers are under "Running it".

A `mac` instance is a full macOS virtual machine on an Apple silicon host,
with the same API, URLs, gateway, templates, sleep and cron as every other
Warren instance. It is for agents that need Xcode and the iOS simulators,
Safari, or macOS applications.

## Running it

On the Mac (Apple silicon, macOS 14 or newer, [Lume](https://github.com/trycua/cua)
0.7.2 or newer, `jq`, the `warren` and `warrend` binaries from
[install.md](install.md) or a release build of the workspace):

1. A stock macOS VM, created once from Apple's restore image with the user
   `lume` and SSH enabled. `warren mac init` creates it when there is none
   (`lume create base --os macos --cpu 4 --memory 4GB --disk-size 50GB
   --ipsw latest --unattended tahoe`: Lume's offline setup creates the
   user, turns Remote Login on and boots once to verify; `--ipsw PATH` for a
   downloaded image, `--unattended sequoia` for a macOS 15 image). By hand:
   the same `lume create` without `--unattended`, `lume run base`, finish
   the macOS setup, enable Remote Login, `lume stop base`.
2. Turn it into Warren's base image: `warren mac init` (`--source base` to
   name an existing stock VM, `--target` for another name than
   `warren-macos-26`, `--force` to replace an existing target, `--dry-run`
   to see the commands). It runs `scripts/mac/build-base.sh`, embedded in
   the binary, which clones the VM, installs the gateway LaunchDaemon and
   launcher, the `pf` isolation rules and your SSH key (`~/.ssh/id_ed25519`,
   generated when missing), replaces the guest user's stock password
   (`lume`) with a random one and restricts SSH to keys, disables sleep and
   automatic updates, and stops the clone. The new password is written to
   `~/.warren/mac/warren-macos-26.password` (`$WARREN_MAC_DIR` to move it,
   mode 600); it is the `sudo` password inside every instance cloned from
   the image and `scripts/mac/debug-guest.sh` reads it from there. Add
   `--with-agents` to also install Node LTS and the `claude`, `codex` and
   `opencode` CLIs in the image (the guest downloads them through the NAT;
   several minutes and about 1 GB), the macOS counterpart of the Linux
   `warren-claude` template. From a checkout, `bash
   scripts/mac/build-base.sh --source base --target warren-macos-26` is the
   same build without the CLI.
3. Start the daemon: `warren up`. It installs the LaunchAgent
   `dev.warren.daemon` from `packaging/launchd/dev.warren.daemon.plist`
   (`warrend --runtime none --mac-base-vm warren-macos-26 --data-dir
   "~/Library/Application Support/Warren" --listen 127.0.0.1:7337`, log in
   `~/Library/Logs/Warren/warrend.log`), waits for `/v1/health`, signs up
   and saves the key to `~/.config/warren/config.json`. `--runtime auto`
   (the default) uses Docker as well when `docker info` works, for `default`
   instances next to the macOS ones; `--port`, `--data-dir`, `--dev` and
   `--no-service` (a plain background process, pid in `<data>/warrend.pid`)
   change the shape; `warren status`, `warren doctor` and `warren down`
   inspect and stop it. Run `warren up` again after `warren mac init` when
   the daemon was started before the base VM existed. The daemon starts
   `lume serve` itself when the API on `127.0.0.1:7777` is not answering
   (`--lume-api` on `warrend` to change) and restarts it if it dies. From a
   checkout without installing: `cargo build --release -p warrend -p
   warren-gateway -p warren-cli`, then `target/release/warren up
   --warrend-bin target/release/warrend`.
4. Create instances from the `warren-macos` template: `warren run --type
   mac -- claude` (a macOS instance bound to the current directory, with
   the agent CLIs when the image was built with `--with-agents`), `warren
   instances create --template warren-macos --wait`, `warren connect
   claude` for the MCP tools, or the API with `"type": "mac"` (the
   dashboard, SDKs and MCP all know the type) or with the template alone,
   since a request without a type takes the template's only type. That is
   how a client that only knows templates, such as a Caret workspace with
   `warren.template` set to `warren-macos`, gets macOS instances. A
   requested disk below the base image's 50 GB is raised to it. Or run the
   whole scenario with `bash scripts/mac/e2e.sh`.

Flags and variables: `--mac-base-vm` / `WARREN_MAC_BASE_VM` (enables the
type), `--lume-api` / `WARREN_LUME_API`, `--lume-bin` / `WARREN_LUME_BIN`,
`--mac-vnc` / `WARREN_MAC_VNC`, `--runtime none` / `WARREN_RUNTIME=none`.
`GET /v1/health` lists `lume` in `runtimes` when the type is on.

What to expect, as measured by `scripts/mac/e2e.sh` on the Phase 0 MacBook
(four runs on 2026-11-19): create to running with a healthy gateway in 21 to
23 s (the clone is instant, the rest is the boot); sleep in 3 to 4 s, which
writes the VM's memory to disk and frees one of the two slots; wake in 5 to 11 s
with every process intact (the marker process keeps its pid); fork of a
running instance in 30 to 37 s (the source is suspended for the copy, the
copy boots, the source resumes); delete of two instances in about 20 s; a
third awake instance answers `409 host_capacity_reached` at create time,
before anything is cloned. A `warrend` restart with a running instance takes
a few seconds and changes nothing inside the VM.
Known limits in this phase: template updates that change the base VM are
refused (`not_supported`); backups are APFS clones that share blocks with
the VM until either side writes, so their reported size is small and real
usage grows as the disk diverges; the guest user is `lume`, its password the
one `build-base.sh` generated (SSH takes keys only, the VM is reachable only
from the Mac itself and blocked between guests by `pf`), and Lume's own
`lume ssh`, `lume shutdown` and `lume restart`, which assume the stock
password, do not work on instances; screen access is not exposed yet.

## Constraints that shape everything

- macOS guests run only on Apple silicon Macs (Virtualization.framework), and
  Apple's software licence allows macOS virtual machines on Apple hardware
  only. Warren can never schedule a `mac` instance onto a Linux node.
- Virtualization.framework runs at most **two macOS guests per host** at a
  time. A Mac is therefore a two-slot node; a sleeping instance must free its
  slot, and the third `create`/`wake` on a full host is refused.
- Lume (trycua, MIT) is the VM manager. Version 0.7.2 or newer is required
  (0.1.x cannot pull images reliably).

## Phase 0 results

Measured on a MacBook Pro (Apple silicon) with Lume 0.7.2 and a VM created
from Apple's `UniversalMac_26.6.2_25G83_Restore.ipsw`, 4 CPU, 4 GB, 50 GB.

| Check | Result |
|---|---|
| Create a macOS VM from an IPSW | works (about 24 GB on disk after install) |
| Clone the VM (APFS clonefile) | instant, blocks shared with the source |
| Two VMs running at once | works (`sandbox` and `second`) |
| Cold boot to SSH | 11 s |
| Memory right after boot | 415 MB for the VM helper process, of a 4 GB cap |
| `warren-gateway` built on macOS | works after two fixes (commits fe1e4b3, 814d152) |
| Gateway inside the VM | `GET /v1/health`, `POST /v1/exec`, `POST /v1/responses` all answer |
| Whole workspace type-checks for `aarch64-apple-darwin` | yes, `warrend` included |

Two facts about Lume's process model came out of this:

- `lume run <vm>` **is** the VM. The process does not return until the VM
  stops, and killing it kills the VM. `lume run --detach` wraps itself in
  `nohup`; VMs started through the HTTP API live inside the `lume serve`
  process.
- `POST /lume/vms/:name/suspend` saves the machine state next to the disk and
  the next run resumes from it, but only for VMs the same `lume serve`
  process started. So Warren drives the API server and lets it host the VMs.

## Lume 0.7.2 facts the runtime relies on

HTTP API, `lume serve` on `127.0.0.1:7777` (`--port` to change):

| Call | Behaviour |
|---|---|
| `GET /lume/host/status` | `{ status, vmCount, maxVMs: 2, availableSlots, version }` |
| `GET /lume/vms`, `GET /lume/vms/:name` | `VMDetails`: `name, os, cpu, memory, diskSize, display, status ("running" or "stopped"), vncUrl, ipAddress, sshAvailable, macAddress, savedState, networkMode, forwards, locationName` |
| `POST /lume/vms/clone` | `{ name, newName, withSavedState? }`; APFS clone of disk, NVRAM and config |
| `POST /lume/vms/:name/run` | body `{ noDisplay, sharedDirectories: [{ hostPath, readOnly }], network, vnc: "enabled"/"disabled", forwards: [guestPort] }`; returns **202** immediately; refuses with 409 when two macOS guests already run |
| `POST /lume/vms/:name/stop` | graceful shutdown, force after a timeout (default 10 s), then waits 5 s for locks |
| `POST /lume/vms/:name/suspend` | pause, save state to disk, VM leaves the running set; `savedState: true` afterwards |
| `PATCH /lume/vms/:name` | `{ cpu, memory, diskSize (grow only), display }`; VM must be stopped |
| `DELETE /lume/vms/:name` | removes the VM directory |

Other facts:

- The guest's address comes from the host DHCP lease for the VM's MAC on the
  NAT network (`192.168.64.0/24`). All VMs on one host share that network;
  nothing isolates them from each other by default.
- Shared directories are virtiofs and macOS guests automount them under
  `/Volumes/My Shared Files/<folder name>`. Lume itself always attaches a
  read-only `lume-config` share.
- `forwards` opens host loopback ports to guest ports over vsock, an
  alternative to the NAT address if lease discovery ever proves slow.
- CLI equivalents exist for everything except suspend: `lume get -f json`,
  `lume ls -f json`, `lume run --detach --display none --vnc disabled
  --shared-dir <host path>:ro`, `lume stop [--force] [--timeout N]`,
  `lume clone`, `lume set`, `lume delete`, `lume ssh`, `lume cp`,
  `lume shutdown` and `lume restart` (graceful, over SSH as `lume`/`lume`).
- Disks are sparse. A clone costs nothing until the guest writes.

## Lessons from the Mac (do not rediscover)

- `lume run` holds the VM in its process; `--detach` backgrounds it. VMs
  started through the API live in the `lume serve` process, and `suspend`
  works only for those. `lume get -f json` prints a one-element array.
- Powering a VM off right after writing files loses them: the first base
  image came up without its LaunchDaemons. `build-base.sh` syncs and shuts
  macOS down from inside; the runtime gives a guest at least 45 s to stop.
- pf `pass` rules must be `flags any no state`, or enabling pf cuts the
  connections that already exist (the installer's SSH session included).
- Apple's `nohup` fails in launchd's system domain ("can't detach from
  console"), so the gateway must run in the user's domain; see "Gateway
  inside the guest". `scripts/mac/debug-guest.sh` prints the domain exec
  runs in and whether `nohup` works there.
- macOS `nc -w N` bounds idle time, not the connect; a SYN that pf drops
  waits on TCP retransmits for over a minute. Use `nc -G N` for a connect
  timeout.
- The shared folder is `/Volumes/My Shared Files/share` in the guest; the
  launcher copies the gateway to `/Users/lume/.warren/` and sources
  `warren.env` (shell-safe `export KEY='value'` lines).
- `*.localhost` does not resolve for `curl` on macOS; the scripts send
  `Host:` headers to `127.0.0.1:<port>`. Browsers resolve it fine.

## Design

### Topology on a Mac host

```
warrend (same binary, built on macOS)
  ├── supervises:  lume serve --port 7777        (hosts every warren-* VM)
  ├── edge proxy → <vm ip>:3737                  (gateway, as for Firecracker)
  └── <data_dir>/mac/<instance id>/share/        (read-only virtiofs share)
        ├── warren-gateway                       (arm64 macOS build)
        └── warren.env                           (instance id, token, driver, model, ...)

Lume VMs: warren-<instance id>, each an APFS clone of a base VM
          (for example warren-macos-26), ~/.lume/<name>/{config.json,disk.img,nvram.bin,state}
```

Linux-only runtimes stay compiled in but become optional at startup: when
Docker is unreachable and `--runtime none` (or no socket) is configured, the
daemon starts without the `default` and `performance` types instead of
exiting.

### Core contract changes (`warren-core`)

- `InstanceType::Mac`, API string `"mac"`. Shapes: `cpu` 2 to 8, `memory_mb`
  4096 to 16384, `disk_gb` fixed by the base image and grow-only.
- `Config`: `lume_api_url` (default `http://127.0.0.1:7777`), `lume_bin`
  (default `lume` on PATH), `mac_base_vm` (default none; the type is disabled
  until set), `mac_vnc: bool` (default off).
- `Error::host_capacity_reached` → 409 `host_capacity_reached`, details
  `{ "max_running": 2, "running": n }`. Distinct from `instance_limit_reached`,
  which is the operator's quota.
- Templates gain `image` values that name a Lume VM for `mac` types
  (`warren-macos` first; a `mac-xcode` template later).

### New crate `warren-runtime-mac`

Implements `Runtime` over the Lume HTTP API with `reqwest` (already a
workspace dependency). Mapping:

| `Runtime` method | Lume |
|---|---|
| `ready` | `GET /lume/host/status` answers and `version >= 0.7.2`; base VM exists and is stopped; else the type is disabled with a logged reason |
| `ensure_image` | `GET /lume/vms/<base>` exists; digest = hash of the base's `config.json` + disk mtime |
| `create` | `POST /lume/vms/clone { name: base, newName: warren-<id> }`; write the share dir; `storage_path` = share dir, `runtime_ref` = VM name |
| `start` | write `warren.env`; `POST run { noDisplay: true, vnc: cfg.mac_vnc, sharedDirectories: [{ share, readOnly: true }] }`; poll `GET` until `status == running` and `ipAddress` set (lease), then poll `http://<ip>:3737/v1/health` (bounded, 90 s); `RunInfo { address: <ip>:3737 }` |
| `stop(grace)` | `POST stop` with `timeout = grace` |
| sleep (lifecycle calls `stop` today) | `POST suspend`; falls back to `stop` when suspend fails; wake is `start`, which resumes from the saved state |
| `destroy` | `DELETE /lume/vms/warren-<id>`; remove the share dir |
| `exec` | the gateway's `POST /v1/exec` with the instance token, exactly as the Firecracker runtime does |
| `logs` | the Lume log for the VM plus the gateway's own log fetched over `exec` |
| `stats` | new gateway `GET /v1/stats` (guest-side `vm_stat`, `sysctl`, load) because every VM lives in one host process |
| `is_running` | `GET /lume/vms/:name` → `status == running` |
| `resize` | `requires_restart()` while running; `PATCH` when stopped |
| `recreate` | refused for `mac` in Phase 1 (`not_supported`): the VM disk **is** the home, so a fresh clone of the base would discard it; a later phase can copy `/Users/lume` across |
| `clone_storage` | `POST /lume/vms/clone { name: warren-<src>, newName: warren-<dst> }`; the clone carries the suspended state when the source sleeps |
| `backup_storage` | `cp -c` (APFS clonefile) of `disk.img`, `nvram.bin`, `config.json` into `dest_path`; instant, no extra space until divergence; VM must not be running (lifecycle already stops first) |
| `restore_storage` | reverse copy into the VM directory while stopped |
| `disk_usage` | allocated blocks of the VM directory (`du -k`) |

The crate is unit-tested on Linux against a small axum mock of the Lume
API (clone, run with delayed `running`, capacity 409, suspend, stop). The
real API is exercised only by `scripts/mac/e2e.sh` on a Mac.

Capacity: `create`, `start`, `wake` and `fork` consult
`GET /lume/host/status` under the per-workspace lock already used for
`--max-awake-instances`, so a full host returns `host_capacity_reached`
before Lume's own 409 is hit.

### Gateway inside the guest

- A LaunchDaemon in the base image (`/Library/LaunchDaemons/dev.warren.gateway.plist`)
  runs `/usr/local/libexec/warren-gateway-launcher.sh` as root, which waits
  for `/Volumes/My Shared Files/share/warren-gateway`, reads `warren.env`,
  and starts the gateway as the `lume` user with `WARREN_HOME=/Users/lume`
  through `launchctl asuser <uid> sudo -n -E -u lume -H`. The detour
  matters: a LaunchDaemon's children live in launchd's system domain, where
  Apple's `nohup` fails with "can't detach from console" (it calls
  `_vprocmgr_detach_from_console`), so an agent's `nohup server &` died
  before it ran. `launchctl asuser` moves the gateway into the user's own
  "Background" domain, the one SSH sessions get, where `nohup` works; `sudo`
  then drops root and keeps the environment. Restart-on-exit is `KeepAlive`;
  launchd tracks the job through `sudo`, which relays signals.
- User names on macOS live in Open Directory, not `/etc/passwd`; the owner
  lookup moves to `nix::unistd::User::from_uid` (the `user` feature is already
  enabled) so health reports `lume` instead of `501`.
- `GET /v1/stats` is added to the gateway (works on Linux too).
- `--init` and `/v1/shutdown` stay Linux-only; stopping is Lume's job.

### Daemon changes (`warrend`)

- `--runtime none` (or an unreachable Docker socket with `--runtime auto`)
  disables the container types instead of failing startup.
- `--lume-api`, `--lume-bin`, `--mac-base-vm`, `--mac-vnc` flags and
  `WARREN_*` variables.
- Supervises `lume serve`: starts it when the API port does not answer,
  restarts it when it exits, and runs the existing reconciler afterwards.
  If the server dies, every hosted VM dies with it; the reconciler marks
  those instances stopped (disk intact) and the next request wakes them.
- Reconciler adopts `warren-*` VMs reported running by Lume at startup.

### Lifecycle rules for `mac`

- Sleep = suspend (memory kept, slot freed). Wake resumes; expected to be
  faster than the 11 s cold boot, to be measured.
- Default idle timeout for `mac` templates: 10 minutes (two slots are
  scarce). Operators can raise it per instance as today.
- Stop = graceful stop (10 s), then force.
- Restart = stop + start (full boot).

### Security on a Mac host

- All guests share the NAT network. The base image installs a `pf` anchor
  that blocks `192.168.64.0/24` except the host gateway (`192.168.64.1`) and
  DNS, so one tenant's VM cannot reach another's gateway port. The e2e script
  verifies this from inside a guest.
- The gateway requires the instance token on every route except health and
  version, as on Linux. The edge still strips the `sk_live_` bearer key.
- Guest credentials, as `scripts/mac/build-base.sh` and the guest-side
  `install.sh` really set them: the host's public key (`--ssh-key`, default
  `~/.ssh/id_ed25519.pub`) goes into `/Users/lume/.ssh/authorized_keys`;
  `/etc/ssh/sshd_config.d/100-warren.conf` sets `PasswordAuthentication no`,
  `KbdInteractiveAuthentication no` and `PubkeyAuthentication yes` (the
  stock `sshd_config` on macOS 13 and newer starts with `Include
  /etc/ssh/sshd_config.d/*`; the installer adds the line at the top when it
  is missing and checks the result with `sshd -T`; macOS starts `sshd` per
  connection, so no restart is needed); the `lume` user's stock password is
  replaced with a random one (`openssl rand -base64 18`, or `--new-password`)
  through `dscl . -passwd`, verified with `dscl . -authonly`, and autologin,
  which stores the password in `/etc/kcpassword`, is re-armed with
  `sysadminctl -autologin set` so the console still logs in. The password
  is stored on the host only, at `$WARREN_MAC_DIR/<target>.password`
  (default `~/.warren/mac`, mode 600); it is what `sudo` inside an instance
  wants and what `debug-guest.sh` uses. The sshd hardening is skipped, with
  a warning, when no key could be installed, so a guest is never left
  reachable through its console only.
- Lume's API server takes requests from any process on the Mac, without
  authentication: anything that can reach `127.0.0.1:7777` can create,
  start, stop or delete VMs, including ones named like Warren instances. At
  startup the runtime lists the VMs named `warren-<instance id>` and warns
  about every one that has no share directory under `<data-dir>/mac/<id>/`,
  which means this daemon did not create it (`lume get <name>` to inspect,
  `lume delete <name>` to remove). Keep the port on loopback and the Mac
  single-tenant.
- VNC stays off unless `--mac-vnc` is set; Lume generates a random password
  per run. Exposing the screen through the edge is Phase 2.

### Host naming

macOS's resolver does not resolve `*.localhost` subdomains for `curl` and
most tools (browsers do). On a Mac host use a wildcard domain that resolves
to the host, for example `--base-domain 127.0.0.1.sslip.io` or `lvh.me`, or
add `/etc/hosts` entries for the few names used in testing.

## Phases

### Phase 1a: runtime and daemon (this host, no Mac needed) — done

1. `warren-core`: `InstanceType::Mac`, shapes, config fields, error code,
   `warren-macos` template.
2. `warren-runtime-mac` with the mock-API unit tests.
3. `warrend`: optional Docker, Lume flags, `lume serve` supervision,
   reconciler adoption.
4. Gateway: `GET /v1/stats`, Open Directory owner names.
5. `cargo check --target aarch64-apple-darwin --workspace` in CI (needs a C
   compiler shim for `aws-lc-sys`; documented in `docs/DEVELOPMENT.md`).

### Phase 1b: base image and end-to-end on the Mac — done

1. `scripts/mac/build-base.sh`: from a stock Lume macOS VM, install the
   LaunchDaemon and launcher, the `pf` anchor, SSH hardening, optional Xcode
   Command Line Tools; stop and name it `warren-macos-26`.
2. `scripts/mac/e2e.sh`: start `warrend` with `--runtime none --mac-base-vm
   warren-macos-26`, then create → health → exec → files → Agent API →
   sleep (`savedState: true`, slot freed) → wake → fork → third create on a
   full host → 409 → delete. Records create-to-ready, wake time, clone time
   and disk delta.
3. Fix what the Mac finds; repeat until the script passes twice in a row.
   Done on 2026-11-19. The Mac found three things: `nohup` is broken in a
   LaunchDaemon's domain (the launcher now moves into the user's domain, see
   "Gateway inside the guest"); macOS `nc -w` does not bound a connect that
   pf silently drops (`-G` does); and create and fork did not consult host
   capacity before cloning (`Runtime::check_capacity` now does).

### Phase 1c: surfaces and docs — done

1. Dashboard type picker, CLI and SDK `--type mac`, MCP tool descriptions.
2. `docs/macos.md` becomes the operator guide (requirements, base image,
   limits, troubleshooting); `CHANGELOG.md`; README roadmap.

### Phase 2 candidates

- Screen: noVNC through the edge (WebSocket to the VM's VNC port).
- Linux guests through Lume on Mac hosts (not subject to the two-guest
  limit) as cheap extra capacity on the same machine.
- One `lume serve` per instance for crash containment and host-side stats.
- `mac-xcode` template with Xcode preinstalled; simulator-ready.
- Multi-node: Mac minis as two-slot nodes behind one `warrend` (the general
  multi-node item on the roadmap).

## Acceptance for Phase 1

On the MacBook used for Phase 0:

- `warrend --runtime none --mac-base-vm warren-macos-26 --base-domain 127.0.0.1.sslip.io --dev`
  starts, reports the `mac` type enabled, and supervises `lume serve`.
- `warren instances create --type mac` reaches `running` with a healthy
  gateway in under 60 s; exec and the Agent API answer.
- Sleep suspends the VM and frees a slot; wake resumes in under 15 s with the
  same processes still running inside.
- With two awake instances a third create returns 409 `host_capacity_reached`.
- Delete removes the VM and the share directory; after a `warrend` restart
  running VMs are adopted, not duplicated.
- From inside one guest, the other guest's gateway port is unreachable.

All met on 2026-11-19 by consecutive runs of `scripts/mac/e2e.sh` (numbers
under "Running it"). The script also stops and restarts `warrend` with a
running instance: the VM keeps running in `lume serve`, the reconciler logs
`adopted running instance`, the marker process keeps its pid and no second
VM appears.

## Open decisions

1. Guest user: keep `lume` (created by the IPSW install with autologin) and
   rotate its password, or create a dedicated `warren` user. Recommendation:
   keep `lume` in Phase 1.
2. Whether to ship Linux-guest support on Mac hosts in Phase 1 or 2.
   Recommendation: Phase 2.
3. Screen access priority. Recommendation: after Phase 1 passes.
