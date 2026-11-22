# Limits

The hard limits of this build, by instance type, with the reason for each one. Some are Apple's, some are Lume's, some are gVisor's or Firecracker's, and some are simply not done yet; the last kind is marked. Measured numbers are in [benchmarks.md](benchmarks.md); the security consequences are in [security-model.md](security-model.md).

## macOS instances (type `mac`)

| Limit | Value | Why |
|---|---|---|
| Guests per host | **2** macOS guests running at once | Virtualization.framework refuses a third; Apple's macOS licence also allows at most two virtual copies per Apple-branded computer. Sleeping instances do not count: suspend frees the slot. The third `create`, `start`, `wake` or `fork` answers `409 host_capacity_reached`. |
| Memory per guest | **4 GB minimum**; shapes up to 16 GB | Apple's minimum for a macOS guest. A 16 GiB Mac runs two guests with room for the host; an 8 GiB Mac realistically runs one. |
| Host hardware | Apple silicon only, macOS 14 or newer | Virtualization.framework for macOS guests exists only there; there is no Intel path and a `mac` instance can never be scheduled on a Linux node. |
| Nested virtualization | **None** inside a macOS guest | Apple does not expose nested virtualization to macOS guests, so no Docker Desktop, no Lume, no Linux VM and no Android emulator with hardware acceleration inside a `mac` instance. The iOS Simulator does not need nested virtualization and is expected to work, but we have not verified it yet. |
| Disk | Fixed to the base image's size (50 GB in the documented setup), **grow only**, up to 500 GB | The VM disk is the persistent home; it cannot shrink, and a requested disk below the base image's size is silently raised to it. Growing is a `PATCH` while stopped. |
| Template update | Refused (`not_supported`) | A fresh clone of a new base VM would discard the disk, which is the home. Copying `/Users/lume` across is future work. |
| Boot and wake times | Create 19 to 23 s, wake 5 to 11 s, fork 30 to 37 s | A macOS boot, not a container start. Sleep keeps memory, so a woken instance resumes where it was. Details and method in [benchmarks.md](benchmarks.md). |
| Screen | **Not exposed yet** | VNC is off unless `--mac-vnc` is set, and even then Lume serves it on the Mac itself, not through the edge. noVNC through the edge is Phase 2. |
| Browser terminal | **None**: `<id>-7681.<base>` answers `502` | `ttyd` is not installed in the macOS base image. Use exec, the files API or SSH from the Mac itself. |
| Network policy | `deny-all` and `allowlist` on `mac` instances are enforced **inside the guest** | The gateway launcher loads a `pf` sub-anchor (`dev.warren/instance`) from `WARREN_NETWORK` that blocks every outbound packet except to the host gateway, where the daemon's egress broker lives. A root process in the guest could undo it, and base images built before this release lack the anchor hook (rebuild with `scripts/mac/build-base.sh`). The daemon installs no host-wide drops on macOS. See [security-model.md](security-model.md). |
| VM manager | Lume 0.7.2 or newer, running as `lume serve` on `127.0.0.1:7777` | Suspend only works for VMs the same `lume serve` started, so Warren drives the API server and lets it host every VM. If that process dies every running VM dies with it; the reconciler marks them stopped (disk intact) and the next request wakes them. |
| Lume's local API | **Unauthenticated** | Any process or user on the Mac that can reach `127.0.0.1:7777` can start, stop, clone or delete every VM. Run Warren on a Mac without untrusted local users and nothing else listening on that interface; see the gaps in [security-model.md](security-model.md). |
| Guest user | `lume` with Lume's default password | `build-base.sh` installs your SSH key and disables password authentication over SSH; the password still exists for the console. The guest is reachable only from the Mac and from the daemon. |
| Backups | APFS clones | Instant and nearly free until the disk diverges; the reported size is small and real usage grows with writes. Restore and backup need the VM stopped or sleeping (the lifecycle does this). |
| Host names | `*.localhost` does not resolve for `curl` on macOS | Use a wildcard domain that points at the host (`127.0.0.1.sslip.io`, `lvh.me`) or `/etc/hosts` entries; browsers resolve `*.localhost` fine. |
| Linux instances on a Mac host | Not in this build | `--runtime none` runs only `mac` instances; a Mac with Docker Desktop can also serve `default` instances, under `runc` (gVisor needs a Linux kernel, so Docker Desktop's VM provides the only boundary). Linux guests through Lume are Phase 2. |

## Licence posture (not legal advice)

Apple's macOS software licence allows up to two additional copies of macOS to run as virtual machines on each Apple-branded computer already running macOS, for purposes including software development and testing ([macOS Tahoe SLA](https://www.apple.com/legal/sla/docs/macOSTahoe.pdf)). Running Warren on your own Mac to give your own agents macOS instances for your own development and testing sits inside that. Offering those virtual machines to other people, for a fee or otherwise, is the "hosting" case in the same licence, which among other things requires that each customer have the whole Apple computer for a minimum period of 24 hours; that is why cloud Mac providers bill by the day. Warren does nothing to enforce either reading: it is software you run, under terms you accept with Apple. Read the licence and, if you are building a service on it, ask a lawyer.

## Linux instances (types `default` and `vm`)

| Limit | `default` (gVisor) | `vm` (Firecracker) |
|---|---|---|
| Host | Linux x86_64 with Docker 24+ | Linux x86_64 with KVM (`/dev/kvm`), **the whole daemon as root**, a Firecracker binary and a guest kernel |
| Syscall compatibility | gVisor implements most of Linux but not all of it: parts of `io_uring`, some `ptrace` uses, nested container runtimes (Docker inside the instance), hardware access (GPUs, `/dev` devices) and some performance tooling are unavailable or slower. Check [gVisor's compatibility reference](https://gvisor.dev/docs/user_guide/compatibility/) for a specific program. | A real Linux kernel; what the kernel build includes is what you get (virtio block and net; no GPU passthrough). |
| Fallback | `runc` when gVisor is missing: the container shares the host kernel with namespaces only. Fine for your own code, not for untrusted code; set `WARREN_RUNTIME=runsc` in production so a missing gVisor is an error. | None: the type is disabled when the binary, kernel or `/dev/kvm` is missing. |
| Sleep | Stops the container; **processes and memory are lost**, the disk stays. Memory checkpoints (gVisor checkpoint/restore) are roadmap. | Pauses the VM and saves its memory (a Firecracker snapshot as large as the guest's RAM next to the disk); wake resumes every process with its pid. Falls back to a cold boot when the snapshot is unusable (other Firecracker version, resized, corrupt). The guest clock is not corrected after a restore. |
| Disk | 2 GB to the shape's maximum (12 / 20 / 40 / 80 GB), grow only; **no filesystem quota** on the bind-mounted home, so an instance can fill the data volume | 2 GB to 100 GB, grow only; a sized ext4 image, so the size is enforced |
| Architecture | x86_64 images only | x86_64 only |
| Resource limits | CPU and memory per shape; 4096 PIDs; disk I/O and network bandwidth are **not** limited | CPU and memory per VM config; I/O and bandwidth not limited |
| Maturity | Unit and end-to-end tested in CI | Experimental; tested on one machine; CI skips it (no stable KVM on hosted runners) |

## Platform-wide

- **Single node.** One `warrend` drives one host. Multi-node placement is roadmap; the `region` field exists on every instance for when it lands.
- **Quotas.** The only per-workspace limit is the awake count (`--max-awake-instances`); combined vCPUs, RAM and disk are not capped ([quotas.md](quotas.md)).
- **TLS.** The daemon speaks plain HTTP; put Caddy, Cloudflare or another proxy in front ([self-hosting.md](self-hosting.md)).
- **Tenancy on one domain.** Instance hosts are subdomains of one registrable domain and browsers treat them as same-site unless the base domain is on the Public Suffix List ([security.md](security.md)).
- **No audit log** beyond the daemon's own logs.
- **Rate limits** exist on signup only.
- **Shapes** are the four CPU/RAM pairs (2/4, 4/8, 4/16, 4/32); a CPU count above 4 is clamped to 4.

## macOS host firewall

The Application Firewall blocks incoming connections to an ad-hoc-signed `warrend` by default, which cuts guests off from the egress broker (`192.168.64.1:7349`) while loopback keeps working. Allow it once with `sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp "$(command -v warrend)"`; `warren doctor` reports the state. See [macos.md](macos.md).
