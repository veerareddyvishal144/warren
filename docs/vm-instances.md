# VM instances (experimental)

Type `vm` runs an instance as a Firecracker microVM with its own Linux kernel instead of a gVisor container. The API is identical; what changes is isolation and host requirements. This type is experimental: it needs root on the host, it is tested on one machine, and it is off unless you enable it. The third type, `mac` (a macOS virtual machine through Lume on an Apple silicon host), is documented separately in [macos.md](macos.md).

## What differs from `default`

| | `default` | `vm` |
|---|---|---|
| Boundary | gVisor user-space kernel on the shared host kernel | A hardware-virtualised microVM with its own guest kernel |
| Disk | up to the shape's maximum (12 / 20 / 40 / 80 GB) | up to 100 GB on every shape |
| `runtime` field | `docker` | `firecracker` |
| Exec | `docker exec` | the gateway's `POST /v1/exec` inside the guest, authenticated with the per-instance token |
| Boot time | about a second | a few seconds (kernel boot plus the gateway) |
| Sleep and wake | stop the container, keep the disk; wake is a fresh start | **sleep keeps the memory**: the guest is paused and snapshotted, wake resumes it with every process and pid intact ([below](#sleep-with-memory)) |
| I/O limits | none | per-shape token-bucket limits on the disk and the NIC ([below](#rate-limits)) |

## Host requirements

- Linux x86_64 with KVM: `/dev/kvm` must exist and be usable by the daemon. Nested virtualisation works (the development host is itself a VM).
- The daemon runs as **root**: it creates tap devices, the `warren-fc0` bridge (`172.30.0.0/24`) and NAT rules for guest egress.
- A Firecracker binary (v1.18 is what the development host uses) and a guest kernel image (an uncompressed `vmlinux`, 6.1 series tested) with virtio block and net drivers built in. The `jailer` binary from the same release when you want the jail ([below](#the-jailer)).
- `e2fsprogs` (`mkfs.ext4`, `debugfs`), `iproute2` (`ip`) and `iptables` on the host.
- Docker is still required: the template image is exported from Docker to build the root filesystem.
- Free disk for sleeping instances: a sleeping `vm` instance keeps a memory file as large as its RAM next to its disk (4 GB for the 2/4 shape, 32 GB for 4/32).

Fetch the binary and kernel with the helper script. It downloads the Firecracker v1.18.0 release for your architecture and the newest 6.1.x guest kernel published by the Firecracker CI, verifies sizes and the release checksums, and writes `DEST/firecracker` and `DEST/vmlinux`:

```bash
sudo scripts/fetch-firecracker-assets.sh                       # DEST defaults to /var/lib/warren/firecracker
sudo scripts/fetch-firecracker-assets.sh /opt/warren/fc        # or any directory
FC_VERSION=v1.18.0 KERNEL_SERIES=6.1 scripts/fetch-firecracker-assets.sh ./assets   # pin versions, no root needed for a local directory
```

## Enable the runtime

Flags or environment variables on `warrend`:

| Flag | Variable | Meaning |
|---|---|---|
| `--firecracker` | `WARREN_ENABLE_FIRECRACKER=true` | Register the `vm` runtime |
| `--firecracker-bin PATH` | `WARREN_FIRECRACKER_BIN` | The binary (default `/usr/local/bin/firecracker`) |
| `--kernel PATH` | `WARREN_KERNEL_IMAGE` | The guest kernel |
| `--firecracker-jailer PATH` | `WARREN_FIRECRACKER_JAILER` | Launch every microVM through the jailer at this path (off by default) |
| `--firecracker-jail-uid N`, `--firecracker-jail-gid N` | `WARREN_FIRECRACKER_JAIL_UID`, `WARREN_FIRECRACKER_JAIL_GID` | The unprivileged ids the jailer drops to (default 61000 for both; they need not exist in `/etc/passwd`) |
| `--firecracker-no-rate-limits` | `WARREN_FIRECRACKER_NO_RATE_LIMITS=true` | Run microVMs without the per-shape disk and network limits |

```bash
sudo target/release/warrend --dev --data-dir ./data --firecracker --kernel /var/lib/warren/vmlinux --firecracker-bin /usr/local/bin/firecracker
curl -s http://api.warren.localhost:7337/v1/health
# {"healthy":true,"version":"0.1.0","runtimes":["docker","firecracker"],"time":...}
```

If the binary, kernel or `/dev/kvm` is missing the daemon logs a warning, leaves `vm` disabled and keeps serving `default` instances; `type: "vm"` then answers `400 not_supported`. The same happens when `--firecracker-jailer` names a missing binary, a uid or gid of 0, or a data directory whose `firecracker/jail` would sit on another filesystem than `instances/` (the jail hard-links instance files).

## Create a VM instance

```bash
curl -s -X POST http://api.warren.localhost:7337/v1/instances -H "Authorization: Bearer $WARREN_API_KEY" -H 'Content-Type: application/json' -d '{
  "template": "warren-base",
  "type": "vm",
  "name": "isolated build",
  "resources": { "cpu": 4, "memory": 8, "disk": 50 }
}'
```

Everything else works the same: URLs, signed URLs, the files API, exec, sleep and wake, fork, backups, resize (disk may only grow; CPU and memory changes need a stop and start), update and delete. All three built-in templates allow `vm`; a workspace template allows it unless it restricts `types`.

## How it is built

1. The template image is exported from Docker into a directory and packed into a sized ext4 image with `mkfs.ext4 -d` under `<data>/instances/<id>/vm/rootfs.ext4` (built as `rootfs.ext4.tmp` and renamed when complete).
2. A tap device with a deterministic name and MAC is attached to the `warren-fc0` bridge; the guest's address on `172.30.0.0/24` is passed on the kernel command line with `ip=`, and NAT gives it egress (`deny-all` and `allowlist` drop the guest's forwarded traffic; an `allowlist` guest reaches the daemon's egress broker on `172.30.0.1` and nothing else directly).
3. Firecracker boots the configured kernel with the rootfs; `warren-gateway` runs as PID 1 (`--init`), mounts what it needs, starts `ttyd` and serves the API on 3737. The daemon waits for `GET /v1/health` before reporting `running`, exactly as for containers.
4. The VM's API socket (`vm/firecracker.sock`), config, pid and console log live next to the rootfs; `GET /v1/instances/{id}/logs` returns the console. The daemon keeps Firecracker's API on (no `--no-api`) because sleep uses it.

## Sleep with memory

`POST /v1/instances/{id}/sleep` on a `vm` instance saves the guest's memory instead of powering it off, the way `mac` instances suspend:

1. `sync` runs inside the guest through the gateway, so the disk is consistent with the memory about to be saved (for forks and backups taken while asleep, and for a cold boot should the snapshot become unusable).
2. `PATCH /vm {"state": "Paused"}` stops the vCPUs; `PUT /snapshot/create {"snapshot_type": "Full"}` writes the microVM state to `<data>/instances/<id>/vm/snapshot/vmstate` and the guest memory to `.../snapshot/mem`. Firecracker drains and `fsync`s the disk during this step.
3. The daemon writes `.../snapshot/meta.json` next to them: the Firecracker version that took the snapshot (`vmm_version` from the API), the shape, the guest address and tap name, the memory size and the time.
4. The paused process is killed, the tap is deleted and the egress rule removed. The instance reports `sleeping`, `is_running` is false, and nothing of it runs on the host.

The memory file is exactly as large as the guest's RAM: 4 GiB for the 2/4 shape, 8 GiB for 4/8, 16 GiB for 4/16, 32 GiB for 4/32. `disk_used_bytes` on the instance object includes it while the instance sleeps (the daemon refreshes the figure periodically). Writing it takes as long as your disk takes to write that much; the lifecycle holds the instance's lock meanwhile, so a request arriving during the sleep waits and then wakes the instance.

Wake (`POST .../start`, a request to the instance URL, an exec or a cron firing) restores the snapshot:

1. The tap is recreated with the same name and bridge membership, so the guest's static address keeps working; the egress rule for the instance's network mode is reinstalled.
2. A fresh Firecracker process is started with the API socket only (no config file), and `PUT /snapshot/load {"snapshot_path", "mem_backend": {"backend_type": "File", "backend_path"}, "enable_diff_snapshots": false, "resume_vm": true}` maps the memory file and resumes the guest.
3. The daemon waits for the gateway port as after a boot, then removes the snapshot files: a snapshot is single-use (the restored process keeps the memory file mapped until it exits, so the space is released when the VM next stops).

Fallbacks and failures, all logged by `warrend`:

- **Cold boot with a warning** when the snapshot is missing, incomplete (no `meta.json`, an empty `vmstate`, a `mem` smaller than the guest's RAM), taken by a different Firecracker version than `--firecracker-bin` reports with `--version`, or taken at a different shape, address or tap. The files are discarded and the VM boots from its disk; processes from before the sleep are gone, files are intact (the pre-sleep `sync` makes the disk consistent).
- **Cold boot with an error log** when Firecracker itself rejects `PUT /snapshot/load` (its own version check, a corrupt file): the fault message is logged, the snapshot discarded, the VM booted cold.
- **Start fails, snapshot kept** when the restore fails for another reason (the process did not start, the API socket did not answer, the gateway did not come up after the resume): the instance goes to `failed` with `snapshot_kept` in the error details, the files stay for the next `start` to retry, and `POST .../stop` discards them.
- `stop`, `resize` to another shape, `update` (a rebuilt root filesystem) and `restore` from a backup discard a saved snapshot; `delete` removes it with the rest of the instance. The snapshot is never copied by `fork`: the memory holds the source's address, hostname, instance token and open connections, so a restored clone would be the source under another id. A fork of a sleeping `vm` instance copies the disk and boots fresh, exactly as before.

Caveats of a restored guest:

- Its wall clock continues from the moment of the snapshot until something in the guest resyncs it (the gateway does not; Firecracker's own guidance is a guest agent or `chronyd`). Timers that measure elapsed time are not affected.
- TCP connections that were open at sleep time are dead on the other side; software inside the guest sees them time out or reset on the next packet, as after a laptop suspend.
- Snapshots are tied to the host's CPU model and kernel: a snapshot taken on one host restores on the same host, or on one with the same CPU family and an equal or newer kernel, per Firecracker's documentation. Warren keeps snapshots on the host that took them.

## The jailer

By default `warrend` runs `firecracker` directly, as root, exactly as previous releases did. `--firecracker-jailer /usr/local/bin/jailer` (`WARREN_FIRECRACKER_JAILER`) puts every microVM in Firecracker's jailer instead:

```
jailer --id <instance id> --exec-file <firecracker> --uid 61000 --gid 61000 --chroot-base-dir <data>/firecracker/jail -- --api-sock /run/firecracker.socket [--config-file /config.json]
```

The jailer builds `<data>/firecracker/jail/firecracker/<instance id>/root`, copies the Firecracker binary in, creates `/dev/kvm`, `/dev/net/tun` and `/run` owned by the jail user, chroots, drops to the uid and gid and execs Firecracker in place (no `--daemonize`, no `--new-pid-ns`, no `--netns`: the tap lives in the host's namespace, and the pid the daemon spawned is the VM's pid). Before each launch `warrend` wipes the instance's jail and hard-links the kernel (`/vmlinux`), the root filesystem (`/rootfs.ext4`, chowned to the jail user) and, on a restore, the snapshot files (`/snapshot/vmstate`, `/snapshot/mem`) into it, and writes a config with those jail-relative paths; snapshots are written to `/snapshot` inside the jail and moved to the instance directory. The tap is created with `ip tuntap add ... user 61000 group 61000` so the unprivileged process may attach to it. The jail is removed when the VM stops. Hard links need one filesystem for `<data>/firecracker/jail` and `<data>/instances`, which `ready` checks; a kernel on another filesystem is copied once under `<data>/firecracker/jail/.kernel/`. The daemon reaches the jailed API socket (`<root>/run/firecracker.socket`, often longer than the 108 bytes `connect(2)` accepts) through its directory's descriptor, so a deep data directory is fine. A plain launch is different: Firecracker binds `<data>/instances/<id>/vm/firecracker.sock` itself and cannot take a long path, so `create` refuses with `invalid_request` when that path is over about 100 bytes (use a shorter `--data-dir` or the jailer).

Seccomp stays at Firecracker's default (on): the daemon never passes `--no-seccomp` or a custom filter. Cgroups are not configured by the jailer (`--cgroup` is not used); the per-shape rate limits bound the VM's I/O instead, and vCPU and memory are fixed by the shape. The uid and gid are plain numbers (`--firecracker-jail-uid`, `--firecracker-jail-gid`); 0 is refused.

## Rate limits

Each microVM's root drive and NIC carry Firecracker token-bucket rate limiters keyed by the shape; the daemon writes them into the VM config (`rate_limiter` on the drive, `rx_rate_limiter` and `tx_rate_limiter` on the interface) and Firecracker enforces them in the device emulation:

| Shape (vCPU / GB) | Disk bandwidth | Disk ops | Network, each direction |
|---|---|---|---|
| 2 / 4 | 100 MB/s | 4,000 IOPS | 100 Mbit/s |
| 4 / 8 | 200 MB/s | 8,000 IOPS | 200 Mbit/s |
| 4 / 16 | 300 MB/s | 12,000 IOPS | 300 Mbit/s |
| 4 / 32 | 400 MB/s | 16,000 IOPS | 400 Mbit/s |

Buckets refill every 100 ms (bursts are at most a tenth of a second's worth) with a one-time initial burst of one second's worth. A shape outside this table scales with its vCPU count (one tier per two vCPUs, at least the first and at most the fourth). `--firecracker-no-rate-limits` (`WARREN_FIRECRACKER_NO_RATE_LIMITS=true`) leaves every device unlimited; the setting applies to VMs started after it, and a restored snapshot keeps the limits it was taken with.

## Testing

The end-to-end suite has `vm_*` tests that run only when the daemon reports the Firecracker runtime: `vm_create_exec` (create, exec, URL, delete) and `vm_sleep_keeps_memory` (a `nohup` marker process survives sleep and wake with the same pid, the VM process is gone and the memory file is present while asleep, the snapshot is removed after the restore, a stop discards it and the next start boots cold):

```bash
scripts/e2e.sh --vm              # builds binaries and images, then runs the suite including vm_* under sudo
make e2e-vm                      # sudo cargo test -p warren-e2e -- --ignored --test-threads=1 vm_
WARREN_E2E_VM=1 WARREN_E2E_KERNEL=/path/to/vmlinux WARREN_E2E_FIRECRACKER_BIN=/path/to/firecracker \
  sudo -E cargo test -p warren-e2e -- --ignored --test-threads=1 vm_
WARREN_E2E_FIRECRACKER_JAILER=/path/to/jailer ...   # the same scenario through the jailer
```

[TESTING.md](TESTING.md) describes the suite and the vm scenario in detail.

CI runs the suite with `--skip vm_` because hosted runners do not offer a stable KVM setup.

## Limitations

- Experimental: expect rough edges, and do not run it as the only type on a production host yet.
- Root is required for the whole daemon, not only for the runtime; the jailer confines the VM process, not the daemon.
- A sleeping instance holds a memory file as large as its RAM on the data volume.
- The guest clock is not corrected after a restore (see the caveats above).
- Sleep with memory and wake were verified on a KVM host (Azure, nested virtualization): a 4 GB guest sleeps in 24 to 25 s, bounded by writing its memory file, and resumes in 0.2 s with its processes and pids intact ([benchmarks.md](benchmarks.md)). The same scenario through the jailer (`WARREN_E2E_FIRECRACKER_JAILER`) passes too: a jailed guest sleeps with its memory and wakes with its processes intact, with the two vm scenarios taking 115 s together on that host.
- x86_64 only, like the images.
