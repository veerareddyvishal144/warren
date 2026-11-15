# Warren design

Warren gives every agent or end user a persistent Linux computer with an agent harness installed, reachable over HTTPS at a URL derived from its id, that sleeps when idle and bills disk only. One Rust binary (`warrend`) runs the whole single-node platform: Sandbox API, edge proxy, lifecycle, metering and cron. A second binary (`warren-gateway`) runs inside every instance.

## Crates

Warren is a Cargo workspace. Every crate depends on `warren-core` and nothing else inside the workspace, except the binaries that wire them together.

| Crate | Responsibility |
|---|---|
| `warren-core` | Shared types, async traits (`Store`, `Runtime`, `Lifecycle`, `Biller`, `TemplateService`, `Signer`, `Authenticator`), errors, shapes, rates, config, ids, the signed-URL signer, the API-key authenticator and JSON conventions. Depends only on external crates. |
| `warren-store` | SQLite persistence with `rusqlite` (bundled). Migrations embedded. WAL mode, busy timeout, one writer behind a mutex, reads on a blocking pool. |
| `warren-runtime-docker` | Default-type instances as Docker containers through `bollard`; runtime `runsc` when the daemon offers it (`--runtime auto`), else `runc`. Two bridge networks: `warren-net` (egress) and `warren-internal` (`internal: true`, deny-all). Bind-mounts `<DataDir>/instances/<id>/home` at the template's `mount_path`. |
| `warren-runtime-firecracker` | VM-type instances: export the template image to a directory, `mkfs.ext4 -d` into a sized rootfs, boot the configured kernel with the Firecracker binary, tap device on bridge `warren-fc0` (172.30.0.0/24) with NAT, guest IP via kernel `ip=`; the gateway runs as PID 1 (`--init`). Needs root and `/dev/kvm`. |
| `warren-templates` | Built-in catalog seeding, workspace templates, pins (`name@rev`), image pulls through the runtime. |
| `warren-billing` | Rates, per-minute metering with fractional carry, wallet ledger, one-day-of-balance rule, awake limits ladder, exhausted-wallet stop. |
| `warren-lifecycle` | The only mover of `Instance.status`. Create/start/stop/sleep/wake/restart/resize/update/fork/delete, exec/logs/metrics, backups, idle sleeper, reconciler at startup (adopt running containers, fix transitional states), metrics sampler, disk usage refresh. Per-instance async mutex. |
| `warren-edge` | Host-header dispatch, instance auth (key, signed URL, cookie), wake-on-request, reverse proxy on hyper with WebSocket upgrade passthrough, CORS. |
| `warren-api` | `/v1` axum routes. Thin: validate, call Lifecycle/Biller/Templates/Store, write JSON. |
| `warren-cron` | Polls due crons every 30 s, wakes the instance, posts the prompt to the gateway, records the result, computes the next run from the cron expression and timezone. |
| `warren-dashboard` | Embedded single-page UI (vanilla JS, `include_str!`): key login, instances, create, lifecycle buttons, terminal via signed URL, exec console, chat, wallet and ledger, dev top-up. |
| `warrend` | The daemon binary: config from flags and `WARREN_*` env, wiring, graceful shutdown. |
| `warren-gateway` | In-instance binary (axum): Agent API, files API, exec, health, supervises `ttyd` on 7681, acts as init in microVMs. Drivers: echo, openai, command. |
| `warren-sdk` | Rust client for both APIs, used by the CLI, the MCP server and the e2e tests. |
| `warren-cli` | `warren` command over the SDK (clap). |
| `warren-mcp` | MCP server exposing instances as tools (`rmcp`). |
| `warren-e2e` | Integration tests that run the real daemon against Docker; `cargo test -p warren-e2e -- --ignored`. |
| `sdk/python`, `sdk/typescript` | One-file clients for customers who do not write Rust. |

## Storage layout

```
<DataDir>/
  warren.db            SQLite
  secret               HMAC secret for signed URLs (0600)
  instances/<id>/home  persistent disk (bind-mounted at mount_path)
  instances/<id>/vm/   rootfs.ext4, firecracker socket and logs (vm type)
  backups/<id>/<backupId>.tar.gz
```

## Metering rules

- Monthly running price = `cpu * VCPUMonthMicros + memory * RAMGBMonthMicros + disk * DiskGBMonthMicros`, compute part multiplied by the type multiplier (`vm` 2.5x). Disk-only price = `disk * DiskGBMonthMicros`.
- Per minute = monthly / 43,800. The biller charges whole minutes elapsed since `MeteredThrough`, carrying fractional micros in `MeterCarryMicros`.
- Running and transitional states (`running starting restarting updating stopping`) bill the full rate; `sleeping stopped waking` bill disk; `provisioning` is free until `MeterStartedAt` is set the first time the instance reaches `running`.
- Create requires `wallet >= one day of running rate`. When the wallet reaches zero the biller stops every running instance in that workspace with `status_reason: "insufficient_balance"`; they stay stopped until a top-up and an explicit start.
- Ledger entries are written per instance per tick with `kind: compute|disk`, negative micros, and the minutes charged. Top-ups are positive `topup` entries and raise `TopUpTotalMicros`; the free credit is a `credit` entry.

## Sleep and wake in this build

Sleep stops the container and keeps the disk. The instance reports `sleeping`, bills disk only, and any request to any of its URLs or an exec wakes it: the edge holds the request, lifecycle starts the container, waits for the gateway health endpoint, then forwards. Processes and memory from before the sleep are gone; files persist. Memory checkpoints (gVisor checkpoint/restore, Firecracker snapshots) are the next step and must not change the API.

## Security model

- Instances never get public IPs. Every request passes the edge, which authenticates with the workspace key or a signed URL.
- Default type: gVisor when present. `runc` is a fallback for development only; `GET /v1/health` lists the enabled runtimes (`docker`, and `firecracker` when enabled) and the daemon logs at startup which Docker runtime (`runsc` or `runc`) it selected.
- Per-instance env never contains the workspace API key. The gateway's exec endpoint requires the per-instance token, which only the daemon knows.
- Files API confines paths to the home directory. Exec output is capped.
- Private ranges: the Docker networks are ordinary bridges; blocking RFC1918 egress from instances is done with an nftables rule set installed at startup when running as root, and skipped with a warning otherwise.
- Secrets at rest: the HMAC secret and API key hashes only; plaintext keys are shown once.

## Testing strategy

- Unit tests per crate with in-memory fakes for the `Store`, `Runtime` and `Biller` traits.
- `warren-e2e` runs the real daemon against Docker with the built images and walks the whole API: signup, templates, create, exec, edge auth, signed URL, agent chat (echo driver), streaming, files, stop/start, sleep/wake, fork, backup/restore, resize, update, ledger, limits, delete.
- Tests named `vm_*` add the vm-type scenario when root and `/dev/kvm` are available (`cargo test -p warren-e2e -- --ignored vm_`).
