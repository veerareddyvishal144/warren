# Warren design

Warren gives every agent or end user a persistent Linux computer with an agent harness installed, reachable over HTTPS at a URL derived from its id, that sleeps when idle and keeps only its disk. One Rust binary (`warrend`) runs the whole single-node platform: Sandbox API, edge proxy, lifecycle and cron. A second binary (`warren-gateway`) runs inside every instance.

## Crates

Warren is a Cargo workspace. Every crate depends on `warren-core` and nothing else inside the workspace, except the binaries that wire them together.

| Crate | Responsibility |
|---|---|
| `warren-core` | Shared types, async traits (`Store`, `Runtime`, `Lifecycle`, `TemplateService`, `Signer`, `Authenticator`), errors, shapes, config, ids, the signed-URL signer, the API-key authenticator and JSON conventions. Depends only on external crates. |
| `warren-store` | SQLite persistence with `rusqlite` (bundled). Migrations embedded. WAL mode, busy timeout, one writer behind a mutex, reads on a blocking pool. |
| `warren-runtime-docker` | Default-type instances as Docker containers through `bollard`; runtime `runsc` when the daemon offers it (`--runtime auto`), else `runc`. Two bridge networks: `warren-net` (egress) and `warren-internal` (`internal: true`, deny-all and allowlist). Bind-mounts `<DataDir>/instances/<id>/home` at the template's `mount_path`. |
| `warren-runtime-firecracker` | VM-type instances: export the template image to a directory, `mkfs.ext4 -d` into a sized rootfs, boot the configured kernel with the Firecracker binary, tap device on bridge `warren-fc0` (172.30.0.0/24) with NAT, guest IP via kernel `ip=`; the gateway runs as PID 1 (`--init`). Drives Firecracker over its API socket: sleep pauses the guest and writes a full snapshot (memory file as large as the RAM) that the next start loads into a fresh process; optional launch through the jailer (chroot with hard-linked kernel, rootfs and snapshot, unprivileged uid/gid, seccomp on); per-shape token-bucket rate limits on the drive and NIC. Needs root and `/dev/kvm`. |
| `warren-runtime-mac` | Mac-type instances: one macOS VM each on an Apple silicon host through Lume's API server (`lume serve`), which the daemon starts and watches. Each VM is an APFS clone of the base VM (`--mac-base-vm`) named `warren-<id>`, started headless with a read-only shared folder (`<data>/mac/<id>/share`) holding the gateway binary and its environment; a LaunchDaemon in the base image starts the gateway from it. Sleep suspends the VM (memory saved, resumed on wake). Two macOS guests per host; `host_capacity_reached` beyond that. See [macos.md](macos.md). |
| `warren-gateway-client` | The host side of talking to a guest's gateway: wait for its port and health, exec, stats and shutdown with the instance token. Used by the Firecracker and mac runtimes. |
| `warren-templates` | Built-in catalog seeding, workspace templates, pins (`name@rev`), image pulls through the runtime. |
| `warren-lifecycle` | The only mover of `Instance.status`. Create/start/stop/sleep/wake/restart/resize/update/fork/delete, exec/logs/metrics, backups, the awake quota, idle sleeper, reconciler at startup and a repair pass every five minutes (adopt running containers, repair stranded transitional rows, stop runtimes of failed rows), metrics sampler, disk usage refresh. Per-instance async mutex. |
| `warren-edge` | Host-header dispatch, instance auth (key, signed URL, cookie), wake-on-request, reverse proxy on hyper with WebSocket upgrade passthrough, CORS. |
| `warren-api` | `/v1` axum routes. Thin: validate, call Lifecycle/Templates/Store, write JSON. |
| `warren-cron` | Polls due crons every 30 s, wakes the instance, posts the prompt to the gateway, records the result, computes the next run from the cron expression and timezone. |
| `warren-dashboard` | Embedded single-page UI (vanilla JS, `include_str!`): key login, instances, create, lifecycle buttons, terminal via signed URL, exec console, chat, workspace quota. |
| `warrend` | The daemon binary: config from flags and `WARREN_*` env, wiring, graceful shutdown. |
| `warren-gateway` | In-instance binary (axum): Agent API, files API, exec, health, supervises `ttyd` on 7681, acts as init in microVMs. Drivers: echo, openai, command. |
| `warren-sdk` | Rust client for both APIs, used by the CLI, the MCP server and the e2e tests. |
| `warren-cli` | `warren` command over the SDK (clap). |
| `warren-mcp` | MCP server exposing instances as tools (`rmcp`). |
| `warren-e2e` | Integration tests that run the real daemon against Docker; `cargo test -p warren-e2e -- --ignored`. |
| `sdk/python`, `sdk/typescript` | One-file clients for users who do not write Rust. |

## Storage layout

```
<DataDir>/
  warren.db            SQLite
  secret               HMAC secret for signed URLs (0600)
  instances/<id>/home  persistent disk (bind-mounted at mount_path)
  instances/<id>/vm/   rootfs.ext4, firecracker socket and logs (vm type)
  backups/<id>/<backupId>.tar.gz
```

## The awake quota

- `Config.max_awake_instances: Option<u32>` comes from `--max-awake-instances` (`WARREN_MAX_AWAKE_INSTANCES`). `None` means no limit and the API reports `awake_limit: null`.
- Awake states are `provisioning`, `running`, `starting`, `restarting`, `updating`, `stopping` and `waking`. `sleeping`, `stopped`, `failed`, `deleting` and `deleted` never count.
- The lifecycle checks the quota on create, start, wake (edge request, exec, cron firing) and fork. A refused request answers `409 instance_limit_reached` with `details: { "limit", "awake" }` (`Error::instance_limit`).
- The quota is per workspace and the same for every workspace on the daemon. Per-workspace CPU, RAM or disk totals are not enforced; the host's capacity and the per-container limits from each instance's shape are the only other bounds.
- `GET /v1/workspace` adds `awake_limit` and `awake_count` to the workspace object, both computed at request time.

## Sleep and wake in this build

Sleep stops the container and keeps the disk on `default`. The instance reports `sleeping`, holds no CPU or memory, and any request to any of its URLs or an exec wakes it: the edge holds the request, lifecycle checks the awake quota, starts the runtime, waits for the gateway health endpoint, then forwards. On `default` processes and memory from before the sleep are gone and files persist; `mac` (Lume suspend) and `vm` (a Firecracker full snapshot under `instances/<id>/vm/snapshot/`, loaded by a fresh process on wake, single-use, discarded by stop) keep the memory. gVisor checkpoint/restore for `default` is the next step and must not change the API.

## Security model

- Instances never get public IPs. Every request passes the edge, which authenticates with the workspace key, a signed URL or the `warren_session` cookie (cookies only on same-origin requests) and strips those credentials before forwarding, including a workspace key sent as `Authorization: Bearer`. CORS on instance hosts allows the dashboard origin and the instance's own origins only; the signed-URL redirect uses an absolute `Location`. API requests time out after 60 s and the listener has a global concurrency limit.
- Default type: gVisor when present. `runc` is a fallback for development only; `GET /v1/health` lists the enabled runtimes (`docker`, and `firecracker` when enabled) and the daemon logs at startup which Docker runtime (`runsc` or `runc`) it selected.
- Per-instance env never contains the workspace API key. Every gateway route except `GET /v1/health` and `/v1/version` requires the per-instance token, which only the daemon knows; the edge and the cron scheduler add it to the requests they forward, and it is not exported to exec shells, agent tool shells or the terminal.
- Files API confines paths to the home directory. Exec output is capped.
- Tenant isolation on the network: `warren-net` and `warren-internal` are created with `com.docker.network.bridge.enable_icc=false`, so one instance cannot reach another's gateway (3737) or terminal (7681) over the bridge; host-to-container traffic, which the edge and the daemon use, is unaffected. A pre-existing network without the option is recreated at startup when nothing is attached to it; otherwise the daemon logs an error naming the fix (stop the instances, `docker network rm <name>`, restart warrend), `ready` still succeeds so existing instances stay manageable, and `create` refuses new instances on that network. Firecracker taps are isolated bridge ports (`ip link set dev <tap> type bridge_slave isolated on`, verified after attaching), so guests on `warren-fc0` cannot reach each other.
- Egress: when running as root the daemon installs idempotent iptables rules at `ready` (checked with `-C`, inserted with `-I`, left in place on shutdown). For each Warren Docker bridge interface (`com.docker.network.bridge.name` or `br-<id>`) the `DOCKER-USER` chain drops forwarded traffic to `169.254.0.0/16` (the cloud metadata service) always and, unless `--allow-private-egress`, to `10.0.0.0/8`, `172.16.0.0/12` and `192.168.0.0/16`; new connections from the bridge to the host's own non-loopback addresses (from `ip -j addr`, falling back to `/proc/net/fib_trie`) are dropped in `INPUT`, where traffic to a local address is actually delivered. The Firecracker runtime installs the same drops for the `172.30.0.0/24` guest subnet (`FORWARD` ahead of the bridge accepts, host addresses in `INPUT`). When not root the Docker runtime logs one warning and installs nothing; `DockerRuntime::egress_rules_installed()` reports the state. Every container also carries `pids_limit: 4096`. IPv6 is not enabled on the Warren networks. With the egress broker enabled, each bridge also gets an `INPUT` accept for new TCP connections to its gateway on the broker port, inserted after the drops so it is evaluated first; `allowlist` containers live on the internal bridge and reach the internet through the broker only (`warren-broker`: `CONNECT` and plain HTTP to the instance's `allowed_domains`, resolved and connected by the broker, private and reserved destinations refused; `/r/<token>` secret routes with header injection). The lifecycle registers an instance's address, mode, allowlist and routes with the broker when its runtime reports the address and removes it when it stops.
- Signup: `--signup-enabled` (default true) and 10 signups per IP per hour, loopback exempt in `--dev`.
- Secrets at rest: the HMAC secret and API key hashes only; plaintext keys are shown once.

## Testing strategy

- Unit tests per crate with in-memory fakes for the `Store` and `Runtime` traits.
- `warren-e2e` runs the real daemon against Docker with the built images and walks the whole API: signup, templates, create, exec, edge auth, signed URL, agent chat (echo driver), streaming, files, stop/start, sleep/wake, fork, backup/restore, resize, update, the awake quota, delete.
- Tests named `vm_*` add the vm-type scenario when root and `/dev/kvm` are available (`cargo test -p warren-e2e -- --ignored vm_`).
