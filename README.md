# Warren

Warren gives every AI agent, or every end user of your product, a persistent Linux computer of its own: a container under gVisor (or a Firecracker microVM) with a real shell, a home directory that survives restarts, a browser terminal, a files API and an optional built-in agent, all reachable over HTTPS at a URL derived from the instance id. Instances sleep when idle and then cost disk only; a prepaid wallet is metered by the minute; a single API key drives both the control plane and the instances themselves.

Warren is a single Rust binary (`warrend`) that runs the whole platform on one Linux host: Sandbox API, edge proxy, lifecycle, metering and cron. A second small binary (`warren-gateway`) runs inside every instance and serves the Agent API, the files API and exec. Everything is open source under Apache-2.0, and the whole wire contract fits in one document: [docs/API.md](docs/API.md).

## Features

| Feature | What you get |
|---|---|
| Persistent computers | Debian 12 with Node 24, Python 3, git and build tools; `/home/agent` persists across sleep, stop, restart, resize and template updates. |
| One URL per instance | `https://<id>.<base>/` reaches the default port, `https://<id>-<port>.<base>/` reaches any port a process listens on. WebSockets pass through. |
| Two planes, one key | The same `sk_live_...` key authenticates the Sandbox API (`api.<base>/v1`) and every instance URL. Signed URLs and a cookie let browsers in without the key. |
| Auto-sleep and wake on request | Idle instances stop and bill disk only; the first request to any of their URLs wakes them and is held until the gateway answers. |
| Agent API | `POST /v1/responses` with sessions, SSE streaming, cancel and replay, on the built-in agent (any OpenAI-compatible model), OpenCode, or your own command. |
| Files and exec | List, read, write, move, delete and download the home directory over HTTP; run shell commands with a captured exit code, stdout and stderr. |
| Browser terminal | `ttyd` on port 7681 inside every instance, opened with a signed URL from the dashboard or the API. |
| Templates | Three built-in images plus your own: register an image, publish revisions, pin instances to `name@rev`, update in place. |
| Fork, backup, restore, resize | Copy an instance with its disk, take backups without stopping, restore, grow the disk or change the shape. |
| Crons | Scheduled prompts for agent instances; each firing wakes the instance and runs one turn. |
| Prepaid metering | Per-minute charges from monthly list prices, a $5 welcome credit, a ledger, awake-instance limits that grow with top-ups. |
| Isolation | gVisor (`runsc`) for the default type; Firecracker microVMs for the experimental `vm` type. |
| Clients | `warren` CLI, an MCP server for Claude Code and Cursor, a Rust SDK, one-file Python and TypeScript clients, a dashboard. |

## Quickstart (90 seconds)

Prerequisites: a Linux x86_64 host with Docker (24 or newer) that your user can talk to, a Rust toolchain (1.90 or newer), `git`, `curl` and, for the examples below, `jq`.

```bash
git clone https://github.com/veerareddyvishal144/warren.git && cd warren
sudo scripts/install-gvisor.sh    # optional but recommended: runs default instances under gVisor
make images                        # builds warren-gateway and the warren/base, warren/agent, warren/opencode images
make dev-up                        # runs warrend --dev on :7337 with data under ./data
```

In a second shell, sign up, create an instance, run a command and open its terminal:

```bash
export WARREN_API=http://api.warren.localhost:7337/v1

# 1. A workspace, a key and $5 of credit
curl -s -X POST $WARREN_API/signup -H 'Content-Type: application/json' -d '{"name":"me"}'
# {"workspace":{"id":"ws_...","wallet_micros":5000000,...},"api_key":"sk_live_...","key_id":"...","wallet_micros":5000000}
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

`*.localhost` resolves to the loopback address on Linux, so the instance URLs work in curl and in a browser without any DNS setup. The dashboard is at `http://warren.localhost:7337/`; paste the key there to see the instance, its wallet and the ledger. [docs/quickstart.md](docs/quickstart.md) continues with the built-in agent, files, sleep and wake.

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
|     |               resize, update, backups, idle sleeper, reconciler, metrics sampler           |
|     |-- warren-billing     per-minute meter, wallet ledger, one-day rule, awake limits           |
|     |-- warren-templates   built-in catalog, workspace images, revisions and pins               |
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

## Pricing and metering

Warren is prepaid. A workspace has a wallet in integer micro-dollars; signup grants $5 once. Every instance has a monthly list price computed from its shape and type with the default rates, which the operator can change:

| Rate | Default |
|---|---|
| vCPU per month | $0.80 |
| GB of RAM per month | $0.70 |
| GB of disk per month | $0.09 |
| `vm` type | compute (vCPU + RAM) x 2.5, disk unchanged |

`monthly = cpu x 0.80 + memory x 0.70 + disk x 0.09` for the default type. The meter charges whole minutes at `monthly / 43,800` (730 hours a month), carrying fractions forward, and writes a ledger entry per instance per tick. Running and transitional states (`running`, `starting`, `restarting`, `updating`, `stopping`) bill the full rate; `sleeping`, `stopped` and `waking` bill disk only; `provisioning` is free until the instance first reaches `running`.

| Shape (vCPU / GB RAM / GB disk) | Running | Per hour | Sleeping or stopped | `vm` running |
|---|---|---|---|---|
| 2 / 4 / 4 | $4.76 / month | $0.0065 | $0.36 / month | $11.36 / month |
| 4 / 8 / 6 | $9.34 / month | $0.0128 | $0.54 / month | $22.54 / month |
| 4 / 16 / 12 | $15.48 / month | $0.0212 | $1.08 / month | $37.08 / month |
| 4 / 32 / 24 | $27.76 / month | $0.0380 | $2.16 / month | $66.16 / month |

Creating an instance requires one day of its running rate in the wallet (`monthly / 30`, about $0.16 for the smallest shape), else `402 insufficient_balance`. When a wallet reaches zero every running instance in the workspace is stopped with `status_reason: "insufficient_balance"`; they stay stopped until a top-up and an explicit start. The number of awake instances is capped by cumulative top-ups: 3 before any top-up, 10 after the first, 50 at $50, 200 at $250, 1,000 at $1,000. Sleeping and stopped instances never count. In this build the only top-up provider is `dev`, accepted when the daemon runs with `--dev`; Stripe is on the roadmap. Details: [docs/billing.md](docs/billing.md).

## Instance types

| | `default` | `vm` (experimental) | `performance` |
|---|---|---|---|
| Isolation | Container under gVisor (`runsc`); `runc` fallback for development | Firecracker microVM with its own kernel | Reserved for hosts with pinned cores |
| Host needs | Docker | Root, `/dev/kvm`, a Firecracker binary and a kernel | Not available in this build: `not_supported` |
| Compute price | 1x | 2.5x | 4x (future) |
| Disk | 2 GB up to the shape's maximum (12 / 20 / 40 / 80 GB) | 2 GB up to 100 GB | - |
| Exec | `docker exec` | the in-instance gateway, authenticated with a per-instance token | - |
| Sleep and wake | Stop the container, keep the disk, boot on request | Same | - |

Shapes are the four CPU/RAM pairs above (`2/4`, `4/8`, `4/16`, `4/32`); a requested CPU count above 4 is clamped to 4 while keeping the requested memory. See [docs/instances.md](docs/instances.md) and [docs/vm-instances.md](docs/vm-instances.md).

## Clients

- **CLI**: `cargo build --release -p warren-cli` builds `target/release/cli`; install it as `warren`. `warren signup`, `warren instances create --template warren-agent --wait`, `warren instances exec <id> -- uname -a`, `warren chat <id> "hello"`, `warren instances open <id> --port 7681`.
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
- **Allowlist egress**: per-instance outbound rules beyond today's `allow-all` and `deny-all`.
- **Stripe**: real top-ups next to the `dev` provider.
- **Cloudflare edge**: terminate TLS and authenticate at the edge, with the daemon as origin.

## License

Apache-2.0. See [LICENSE](LICENSE).
