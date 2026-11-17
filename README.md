# Warren

Warren gives every AI agent, or every end user of your product, a persistent Linux computer of its own: a container under gVisor (or a Firecracker microVM) with a real shell, a home directory that survives restarts, a browser terminal, a files API and an optional built-in agent, all reachable over HTTPS at a URL derived from the instance id. Instances sleep when idle and keep only their disk; a single API key drives both the control plane and the instances themselves.

Warren is a single Rust binary (`warrend`) that runs the whole platform on one Linux host: Sandbox API, edge proxy, lifecycle and cron. A second small binary (`warren-gateway`) runs inside every instance and serves the Agent API, the files API and exec. Everything is open source under Apache-2.0, and the whole wire contract fits in one document: [docs/API.md](docs/API.md).

## Features

| Feature | What you get |
|---|---|
| Persistent computers | Debian 12 with Node 24, Python 3, git and build tools; `/home/agent` persists across sleep, stop, restart, resize and template updates. |
| One URL per instance | `https://<id>.<base>/` reaches the default port, `https://<id>-<port>.<base>/` reaches any port a process listens on. WebSockets pass through. |
| Two planes, one key | The same `sk_live_...` key authenticates the Sandbox API (`api.<base>/v1`) and every instance URL. Signed URLs and a cookie let browsers in without the key. |
| Auto-sleep and wake on request | Idle instances stop and keep only their disk; the first request to any of their URLs wakes them and is held until the gateway answers. |
| Agent API | `POST /v1/responses` with sessions, SSE streaming, cancel and replay, on the built-in agent (any OpenAI-compatible model), OpenCode, or your own command. |
| Files and exec | List, read, write, move, delete and download the home directory over HTTP; run shell commands with a captured exit code, stdout and stderr. |
| Browser terminal | `ttyd` on port 7681 inside every instance, opened with a signed URL from the dashboard or the API. |
| Templates | Three built-in images plus your own: register any Linux image (Alpine included) and Warren injects its static gateway and wraps the image's own entrypoint, so files, exec and health work without changing the image. Publish revisions, pin instances to `name@rev`, update in place. |
| Fork, backup, restore, resize | Copy an instance with its disk, take backups without stopping, restore, grow the disk or change the shape. |
| Crons | Scheduled prompts for agent instances; each firing wakes the instance and runs one turn. |
| Capacity quota | An optional cap on how many instances a workspace may keep awake at once (`--max-awake-instances`); sleeping and stopped instances never count. |
| Isolation | gVisor (`runsc`) for the default type; Firecracker microVMs for the experimental `vm` type. |
| Clients | `warren` CLI, an MCP server for Claude Code and Cursor, a Rust SDK, one-file Python and TypeScript clients, a dashboard. |

## Quickstart (90 seconds)

Prerequisites: a Linux x86_64 host with Docker (24 or newer) that your user can talk to, a Rust toolchain (1.90 or newer), the musl target for the static gateway (`make musl-setup` installs `musl-tools` and `rustup target add x86_64-unknown-linux-musl`), `git`, `curl` and, for the examples below, `jq`.

```bash
git clone https://github.com/veerareddyvishal144/warren.git && cd warren
sudo scripts/install-gvisor.sh    # optional but recommended: runs default instances under gVisor
make musl-setup                    # one-time: musl-tools and the x86_64-unknown-linux-musl target
make images                        # builds the static warren-gateway and the warren/base, warren/agent, warren/opencode images
make dev-up                        # runs warrend --dev on :7337 with data under ./data
```

In a second shell, sign up, create an instance, run a command and open its terminal:

```bash
export WARREN_API=http://api.warren.localhost:7337/v1

# 1. A workspace and a key
curl -s -X POST $WARREN_API/signup -H 'Content-Type: application/json' -d '{"name":"me"}'
# {"workspace":{"id":"ws_...","name":"me","created_at":...},"api_key":"sk_live_...","key_id":"..."}
export WARREN_API_KEY=sk_live_...      # paste the api_key from the response
AUTH="Authorization: Bearer $WARREN_API_KEY"

# 2. An instance (defaults: template warren-base, 2 vCPU / 4 GB RAM / 4 GB disk)
ID=$(curl -s -X POST $WARREN_API/instances -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"name":"first","auto_sleep":true}' | jq -r .id)
until [ "$(curl -s $WARREN_API/instances/$ID -H "$AUTH" | jq -r .status)" = running ]; do sleep 1; done

# 3. Run a command in it
curl -s -X POST $WARREN_API/instances/$ID/exec -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"command":"uname -a && whoami && node --version && python3 --version"}'
# {"exit_code":0,"stdout":"Linux ...\nagent\nv24...\nPython 3.11...\n","stderr":"","duration_ms":...}

# 4. A browser terminal via a signed URL (valid one hour), then open the url it prints
curl -s "$WARREN_API/instances/$ID/signed-url?port=7681&ttl=3600" -H "$AUTH"
# {"url":"http://ab12cd34ef-7681.warren.localhost:7337/?warren_token=...","expires_at":...}
```

Chrome, Firefox and hosts running systemd-resolved resolve `*.localhost` to the loopback address, so the instance URLs work in a browser and in curl without DNS setup; on other hosts add `--resolve api.warren.localhost:7337:127.0.0.1` (and the same for each instance host) to curl, or add the names to `/etc/hosts`. The dashboard is at `http://warren.localhost:7337/`; paste the key there to see your instances. [docs/quickstart.md](docs/quickstart.md) continues with the built-in agent, files, sleep and wake.

## Architecture

```
          https://api.<base>/v1                https://<id>.<base>/          https://<id>-<port>.<base>/
          Sandbox API (Bearer sk_live_...)      instance, default port        instance, any port
                     |                          key, signed URL or cookie     key, signed URL or cookie
                     |                                     |                            |
+--------------------v-------------------------------------v----------------------------v---------+
| warrend  (one process, one listener, default :7337)                                              |
|                                                                                                  |
|  warren-edge        Host header dispatch, instance auth, wake-on-request, reverse proxy, CORS    |
|     |        |                                                                                   |
|     v        v                                                                                   |
|  warren-api  warren-dashboard                                                                    |
|  /v1 routes  single-page UI at https://<base>/                                                   |
|     |                                                                                            |
|     v                                                                                            |
|  warren-lifecycle   the only mover of instance status: create, start, stop, sleep, wake, fork,   |
|     |               resize, update, backups, idle sleeper, reconciler, metrics sampler, quota    |
|     |-- warren-templates   built-in catalog, workspace images, revisions and pins                |
|     |-- warren-cron        due prompts every 30 s -> POST /v1/responses inside the instance      |
|     v                                                                                            |
|  Runtime trait  --> warren-runtime-docker (gVisor runsc, runc fallback)                          |
|                 --> warren-runtime-firecracker (microVMs, experimental)                          |
|  warren-store   SQLite at <data>/warren.db                                                       |
+-----------------------------------------------|--------------------------------------------------+
                                                | docker / firecracker
            +-----------------------------------v------------------+   +-------------------------+
            | instance ab12cd34ef (container or microVM)           |   | instance cd34ef56ab     |
            |  warren-gateway :3737   Agent API, files, exec, health|   |  ...                    |
            |  ttyd           :7681   browser terminal              |   +-------------------------+
            |  /home/agent            persistent disk =             |
            |                         <data>/instances/<id>/home    |
            +------------------------------------------------------+
```

Every crate depends on `warren-core` (types, traits, errors, config) and on nothing else inside the workspace; `warrend` wires them together. [docs/DESIGN.md](docs/DESIGN.md) describes each crate.

## Capacity and quotas

Warren is free software you host yourself. It does no billing: there is no wallet, no metering and no price on anything; the only limit on a workspace is the host's capacity and, if you set one, an awake quota. Operators who run Warren as a hosted service build their own billing on top of the API, which exposes everything needed for it: the instance list, each instance's status, shape (`resources`) and uptime (`started_at`, `last_activity_at`, per-minute `metrics`).

The host's vCPUs and RAM are the real limit: every awake instance holds its full shape, while sleeping and stopped instances hold only their disk. To keep one workspace from taking the whole machine, start the daemon with `--max-awake-instances N` (`WARREN_MAX_AWAKE_INSTANCES`). Unset means unlimited.

```bash
target/release/warrend --dev --data-dir ./data --max-awake-instances 3
```

The quota counts instances that are `provisioning`, `running`, `starting`, `restarting`, `updating`, `stopping` or `waking`. Create, start, wake and fork answer `409 instance_limit_reached` with `details: { "limit", "awake" }` when the workspace is at the limit; `GET /v1/workspace` returns `awake_limit` (a number, or `null` when unlimited) and `awake_count`. Auto-sleep is how you keep many instances alive with few slots. Details: [docs/quotas.md](docs/quotas.md).

| Shape (vCPU / GB RAM) | Default disk | Disk range, `default` | Disk range, `vm` |
|---|---|---|---|
| 2 / 4 | 4 GB | 2 - 12 GB | 2 - 100 GB |
| 4 / 8 | 6 GB | 2 - 20 GB | 2 - 100 GB |
| 4 / 16 | 12 GB | 2 - 40 GB | 2 - 100 GB |
| 4 / 32 | 24 GB | 2 - 80 GB | 2 - 100 GB |

## Instance types

| | `default` | `vm` (experimental) | `performance` |
|---|---|---|---|
| Isolation | Container under gVisor (`runsc`); `runc` fallback for development | Firecracker microVM with its own kernel | Reserved for hosts with pinned cores |
| Host needs | Docker | Root, `/dev/kvm`, a Firecracker binary and a kernel | Not available in this build: `not_supported` |
| Disk | 2 GB up to the shape's maximum (12 / 20 / 40 / 80 GB) | 2 GB up to 100 GB | - |
| Exec | `docker exec` | the in-instance gateway, authenticated with a per-instance token | - |
| Sleep and wake | Stop the container, keep the disk, boot on request | Same | - |

Shapes are the four CPU/RAM pairs above (`2/4`, `4/8`, `4/16`, `4/32`); a requested CPU count above 4 is clamped to 4 while keeping the requested memory. See [docs/instances.md](docs/instances.md) and [docs/vm-instances.md](docs/vm-instances.md).

## Clients

- **CLI**: `cargo build --release -p warren-cli` builds `target/release/warren`. `warren signup`, `warren instances create --template warren-agent --wait`, `warren instances exec <id> -- uname -a`, `warren chat <id> "hello"`, `warren instances open <id> --port 7681`.
- **MCP server** for Claude Code, Cursor, Claude Desktop and Windsurf: [mcp/README.md](mcp/README.md).
- **Rust SDK**: `warren-sdk`, a client for both the Sandbox API and the Agent API, used by the CLI, the MCP server and the end-to-end tests.
- **Python and TypeScript**: one-file, dependency-free clients for both APIs, including streaming chat: [sdk/python](sdk/python/README.md) (Python 3.8+, standard library only) and [sdk/typescript](sdk/typescript/README.md) (`fetch`-based; Node 20+, Deno, Bun, browsers).
- **Dashboard**: served at the root host; stores the key locally in the browser.
- **For coding agents**: [docs/for-agents.md](docs/for-agents.md) has a prompt to paste and [docs/llms.txt](docs/llms.txt) indexes every page.

## Documentation

[docs/index.md](docs/index.md) lists every page. Start with [quickstart](docs/quickstart.md), [concepts](docs/concepts.md), [instances](docs/instances.md), [agent API](docs/agent-api.md), [self-hosting](docs/self-hosting.md) and [security](docs/security.md). The wire contract is [docs/API.md](docs/API.md); development rules are in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Roadmap

- **Memory checkpoints**: gVisor checkpoint/restore and Firecracker snapshots so sleep keeps processes and memory, not only files. The API does not change.
- **Multi-node scheduler**: place instances across hosts; the `region` field is already on every instance.
- **Allowlist egress**: per-instance outbound rules beyond today's `allow-all` and `deny-all`. 0.1.0 already blocks cloud metadata (`169.254.0.0/16`) and, unless `--allow-private-egress` is set, private ranges host-wide; the allowlist adds per-instance destinations.
- **Cloudflare edge**: terminate TLS and authenticate at the edge, with the daemon as origin.

## License

Apache-2.0. See [LICENSE](LICENSE).
