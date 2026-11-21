# Warren

**Agent sandboxes on hardware you own.** Linux containers (gVisor), Linux microVMs (Firecracker) and real macOS virtual machines (Apple silicon) behind one API, one CLI and one MCP server. Works with Claude Code, Codex, Cursor, Caret and any MCP client.

Every instance is a persistent computer: a shell, a home directory that survives sleep and restarts, a files API, an optional Agent API and an HTTPS URL for every port it listens on. Instances sleep when idle and wake on the next request; macOS instances keep their memory and processes across sleep. One Rust binary (`warrend`) runs the whole platform on one host, a second (`warren-gateway`) runs inside every instance, and the wire contract fits in one document: [docs/API.md](docs/API.md). Apache-2.0. No billing, no meter: you supply the machine.

## Three commands

Install the `warren` binary ([docs/install.md](docs/install.md)), then:

```bash
warren up                 # start the daemon on this machine with the runtimes it finds
warren connect claude     # register Warren's MCP server with Claude Code
warren run -- claude      # create an instance and run Claude Code inside it
```

On an Apple silicon Mac with Lume and a base image, `warren up` serves macOS instances; on a Linux host with Docker it serves gVisor containers. [docs/quickstart.md](docs/quickstart.md) walks through the first instance, [docs/run.md](docs/run.md) covers `warren run`, and [docs/macos.md](docs/macos.md) prepares a Mac.

## Features

| Feature | What you get |
|---|---|
| Persistent computers | Linux: Debian 12 with Node 24, Python 3, git and build tools, `/home/agent` persists across sleep, stop, restart, resize and template updates. macOS: a full macOS guest whose disk is the home. |
| Linux sandboxes on a Mac | Without Docker Desktop: the [smolvm engine](docs/linux-on-mac.md) runs each Linux instance as its own microVM on Apple silicon; Docker Desktop works too. The daemon publishes instance ports on loopback where the host cannot route to containers. |
| Real macOS instances | A macOS VM per instance on an Apple silicon Mac (`warren-macos` template, type `mac`): Xcode, the Simulator, Safari, anything that needs macOS. Same API, URLs, gateway and crons as every other instance. |
| Sleep with memory | macOS instances suspend in 3 to 4 s and resume in 5 to 11 s with every process intact and the same pids. Linux instances keep their disk across sleep; memory checkpoints are on the roadmap. |
| Fork, backup, restore, resize | Copy an instance with its disk, take backups without stopping, restore, grow the disk or change the shape. On macOS forks and backups are APFS clones: instant, sharing blocks until writes diverge. |
| Two guests per Mac | Apple's limit, enforced before anything is cloned: a third macOS instance answers `409 host_capacity_reached`. Sleeping frees a slot, so a Mac serves many instances two at a time. |
| One URL per instance | `https://<id>.<base>/` reaches the default port, `https://<id>-<port>.<base>/` any port. WebSockets pass through; signed URLs let browsers in without the key. |
| Two planes, one key | The same `sk_live_...` key drives the Sandbox API (`api.<base>/v1`) and every instance URL; nothing inside an instance ever sees it. |
| Auto-sleep and wake on request | Idle instances sleep; the first request to any of their URLs wakes them and is held until the gateway answers. |
| Agent API | `POST /v1/responses` with sessions, SSE streaming, cancel and replay, on the built-in agent (any OpenAI-compatible model), OpenCode or your own command. Crons wake an instance and run a turn on a schedule. |
| Files and exec | List, read, write, move, delete and download the home over HTTP; run commands with captured exit code, stdout and stderr. |
| Templates | Built-in Linux and macOS images plus your own: register any Linux image and Warren injects its static gateway. Revisions, `name@rev` pins, in-place updates. |
| Egress allowlist and secret broker | **New in this release**: a per-instance domain allowlist enforced by a host proxy, and provider keys that stay on the host and are injected on the way out. See [docs/security-model.md](docs/security-model.md). |
| Clients | `warren` CLI, MCP server, Rust SDK, one-file Python and TypeScript clients, a dashboard. |

## How it compares

Claude Code's `/sandbox` wraps the Bash tool on your own machine; Docker Sandboxes gives Claude a Linux microVM on a Mac; E2B, Daytona and Modal rent Linux sandboxes by the hour; Daytona now rents Macs by the day. Warren is the one that runs Linux and macOS instances on hardware you own behind one API, and it is also newer, slower to start than the clouds, single node and without a screen for macOS yet. The full table, with sources and an honest list of where Warren is behind: [docs/compare.md](docs/compare.md).

## Instance types

| | `default` | `vm` (experimental) | `mac` |
|---|---|---|---|
| Isolation | Container under gVisor (`runsc`); `runc` fallback for development | Firecracker microVM with its own kernel | macOS VM (Virtualization.framework through Lume); at most two per host |
| Host | Linux x86_64 with Docker | Linux x86_64, root, `/dev/kvm`, a Firecracker binary and a kernel | Apple silicon Mac, macOS 14+, Lume 0.7.2+, a base VM (`--mac-base-vm`) |
| Disk | 2 GB to the shape's maximum (12 / 20 / 40 / 80 GB) | 2 to 100 GB | The base image's size up to 500 GB, grow only |
| Sleep | Stop, keep the disk, boot on request | Same | Suspend; memory and processes survive |
| Screen | Browser terminal (`ttyd`) | Browser terminal | Not yet (Phase 2) |

Shapes are `2/4`, `4/8`, `4/16` and `4/32` vCPU/GB; `mac` needs at least 4 GB. Details in [docs/instances.md](docs/instances.md), [docs/vm-instances.md](docs/vm-instances.md), [docs/macos.md](docs/macos.md) and [docs/limits.md](docs/limits.md).

## Architecture

```
  https://api.<base>/v1          https://<id>.<base>/        https://<id>-<port>.<base>/
  Sandbox API (Bearer key)       instance, default port      instance, any port
            |                              |                           |
+-----------v------------------------------v---------------------------v-----------+
| warrend  (one process, one listener)                                             |
|  warren-edge        host dispatch, auth, wake-on-request, reverse proxy          |
|  warren-api         /v1       warren-dashboard   UI at https://<base>/           |
|  warren-lifecycle   create, start, stop, sleep, wake, fork, backups, quota       |
|  warren-templates   catalog, revisions, pins     warren-cron   scheduled turns   |
|  Runtime trait  -> warren-runtime-docker       (gVisor runsc, runc fallback)     |
|                 -> warren-runtime-firecracker  (microVMs, Linux, root)           |
|                 -> warren-runtime-mac          (macOS VMs through lume serve)    |
|  warren-store       SQLite at <data>/warren.db                                   |
+----------------------------------------|-----------------------------------------+
                                         |
      +----------------------------------v-----------+   +------------------------+
      | instance ab12cd34ef (container, microVM, VM) |   | instance cd34ef56ab    |
      |  warren-gateway :3737  Agent API, files, exec|   |  ...                   |
      |  ttyd :7681 (Linux)    browser terminal      |   +------------------------+
      |  persistent home       the disk              |
      +----------------------------------------------+
```

Every crate depends on `warren-core` and nothing else in the workspace; `warrend` wires them together. [docs/DESIGN.md](docs/DESIGN.md) describes each crate.

## Clients

- **CLI**: `warren up`, `warren connect claude`, `warren run -- <command>`, `warren instances create --template warren-macos --wait`, `warren instances exec <id> -- uname -a`, `warren chat <id> "hello"`, `warren instances open <id> --port 7681`.
- **MCP server** for Claude Code, Codex, Cursor, Claude Desktop and Windsurf: [mcp/README.md](mcp/README.md). Caret workspaces target a template by name (`warren.template: warren-macos`).
- **Rust SDK** (`warren-sdk`), one-file **Python** and **TypeScript** clients ([sdk/python](sdk/python/README.md), [sdk/typescript](sdk/typescript/README.md)), and a **dashboard** at the root host.
- **For coding agents**: a prompt to paste in [docs/for-agents.md](docs/for-agents.md) and an index in [docs/llms.txt](docs/llms.txt).

## Status

- **Version 0.1.0, unreleased.** No tag and no published binaries yet; the launch work is tracked in [docs/launch-checklist.md](docs/launch-checklist.md).
- **Single node.** One daemon drives one host. Multi-node placement is roadmap.
- **Linux `default` type** is exercised end to end in CI under Docker. **`vm` (Firecracker)** is experimental and tested on one machine.
- **macOS Phase 1 is verified on one Mac**: a MacBook Pro (Apple silicon, 16 GiB) running `scripts/mac/e2e.sh` on 2026-11-19, twice in a row: create, exec, sleep and wake with processes intact, daemon restart with the VM adopted, fork, guest isolation, host capacity, delete. The numbers and their method are in [docs/benchmarks.md](docs/benchmarks.md). It has not run on a second Mac, a Mac mini, or any other macOS host version.
- **New in this release and not yet exercised beyond the development machines**: `warren up`, `warren connect` and `warren run`; the egress allowlist and secret broker; the install path in [docs/install.md](docs/install.md). Treat them as fresh.
- **Not done**: a screen for macOS instances, host-side egress enforcement for macOS guests (the guest applies its own policy with `pf`), memory checkpoints for Linux, TLS in the daemon (use a proxy), an audit log. The complete list is [docs/limits.md](docs/limits.md).

## Documentation

[docs/index.md](docs/index.md) lists every page. Start with [install](docs/install.md), [quickstart](docs/quickstart.md), [run](docs/run.md) and [concepts](docs/concepts.md); then [instances](docs/instances.md), [templates](docs/templates.md), [macOS](docs/macos.md), [security model](docs/security-model.md), [limits](docs/limits.md) and [self-hosting](docs/self-hosting.md). The wire contract is [docs/API.md](docs/API.md); development rules are in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Roadmap

- **Screen for macOS instances**: noVNC through the edge, then a screenshot and input API for agents that need to see.
- **Memory checkpoints for Linux**: gVisor checkpoint/restore and Firecracker snapshots so sleep keeps processes and memory on every type (`mac` already does through Lume suspend). The API does not change.
- **Linux guests on Mac hosts**, outside the two-guest limit, as cheap extra capacity on the same machine.
- **Harden the Mac host**: host-side `pf` egress drops for the NAT subnet (today each guest enforces its own policy), one `lume serve` per instance, and in time a runtime on Virtualization.framework without Lume.
- **Secrets at rest**: encrypt instance env and brokered secrets with a key kept outside the data directory.
- **Multi-node scheduler**: place instances across hosts; the `region` field is already on every instance.
- **Cloudflare edge**: terminate TLS and authenticate at the edge, with the daemon as origin.

## License

Apache-2.0. See [LICENSE](LICENSE).
