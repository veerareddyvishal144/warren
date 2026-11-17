# VM instances (experimental)

Type `vm` runs an instance as a Firecracker microVM with its own Linux kernel instead of a gVisor container. The API is identical; what changes is isolation and host requirements. This type is experimental: it needs root on the host, it is tested on one machine, and it is off unless you enable it.

## What differs from `default`

| | `default` | `vm` |
|---|---|---|
| Boundary | gVisor user-space kernel on the shared host kernel | A hardware-virtualised microVM with its own guest kernel |
| Disk | up to the shape's maximum (12 / 20 / 40 / 80 GB) | up to 100 GB on every shape |
| `runtime` field | `docker` | `firecracker` |
| Exec | `docker exec` | the gateway's `POST /v1/exec` inside the guest, authenticated with the per-instance token |
| Boot time | about a second | a few seconds (kernel boot plus the gateway) |
| Sleep and wake | stop the container, keep the disk | stop the VM, keep the disk; snapshots are a roadmap item |

## Host requirements

- Linux x86_64 with KVM: `/dev/kvm` must exist and be usable by the daemon. Nested virtualisation works (the development host is itself a VM).
- The daemon runs as **root**: it creates tap devices, the `warren-fc0` bridge (`172.30.0.0/24`) and NAT rules for guest egress.
- A Firecracker binary (v1.18 is what the development host uses) and a guest kernel image (an uncompressed `vmlinux`, 6.1 series tested) with virtio block and net drivers built in.
- `e2fsprogs` (`mkfs.ext4`, `debugfs`), `iproute2` (`ip`) and `iptables` on the host.
- Docker is still required: the template image is exported from Docker to build the root filesystem.

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

```bash
sudo target/release/warrend --dev --data-dir ./data --firecracker --kernel /var/lib/warren/vmlinux --firecracker-bin /usr/local/bin/firecracker
curl -s http://api.warren.localhost:7337/v1/health
# {"healthy":true,"version":"0.1.0","runtimes":["docker","firecracker"],"time":...}
```

If the binary, kernel or `/dev/kvm` is missing the daemon logs a warning, leaves `vm` disabled and keeps serving `default` instances; `type: "vm"` then answers `400 not_supported`.

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
2. A tap device with a deterministic name and MAC is attached to the `warren-fc0` bridge; the guest's address on `172.30.0.0/24` is passed on the kernel command line with `ip=`, and NAT gives it egress (`deny-all` removes it).
3. Firecracker boots the configured kernel with the rootfs; `warren-gateway` runs as PID 1 (`--init`), mounts what it needs, starts `ttyd` and serves the API on 3737. The daemon waits for `GET /v1/health` before reporting `running`, exactly as for containers.
4. The VM's socket, pid and console log live next to the rootfs; `GET /v1/instances/{id}/logs` returns the console.

## Testing

The end-to-end suite has `vm_*` tests that run only when the daemon reports the Firecracker runtime:

```bash
scripts/e2e.sh --vm              # builds binaries and images, then runs the suite including vm_* under sudo
make e2e-vm                      # sudo cargo test -p warren-e2e -- --ignored --test-threads=1 vm_
WARREN_E2E_VM=1 WARREN_E2E_KERNEL=/path/to/vmlinux WARREN_E2E_FIRECRACKER_BIN=/path/to/firecracker \
  sudo -E cargo test -p warren-e2e -- --ignored --test-threads=1 vm_
```

[TESTING.md](TESTING.md) describes the suite and the vm scenario in detail.

CI runs the suite with `--skip vm_` because hosted runners do not offer a stable KVM setup.

## Limitations

- Experimental: expect rough edges, and do not run it as the only type on a production host yet.
- Root is required for the whole daemon, not only for the runtime.
- Memory snapshots are not implemented; sleep is a VM stop and wake is a boot.
- x86_64 only, like the images.
