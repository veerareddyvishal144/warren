# Warren

**Sandboxes for coding agents, on hardware you own.** One daemon gives Claude Code, Codex, Cursor, Caret or any MCP client persistent computers to work in: Linux containers (gVisor), Linux microVMs (Firecracker) and real macOS virtual machines (Apple silicon), all behind one API, one CLI and one MCP server. Each instance has a shell, a home that survives sleep and restarts, a files API, a URL for every port and, on macOS, a screen the agent can look at and click. Apache-2.0, no account, nothing leaves your machine.

## 1. Install

Apple silicon Mac (macOS 14 or newer) or x86_64 Linux. Pick one:

```sh
# Homebrew (macOS or Linux). Homebrew 5 and newer ask you to trust a third-party tap once.
brew tap veerareddyvishal144/warren && brew trust veerareddyvishal144/warren
brew install veerareddyvishal144/warren/warren

# or the install script (puts warren, warrend, warren-mcp and warren-gateway in /usr/local/bin or ~/.local/bin)
curl -fsSL https://raw.githubusercontent.com/veerareddyvishal144/warren/main/scripts/install.sh | sh

# or from source
cargo build --release -p warrend -p warren-cli -p warren-mcp -p warren-gateway
```

What the host needs, checked for you by `warren doctor`:

- **macOS**: [Lume](https://github.com/trycua/cua) 0.7.2 or newer for macOS instances (`warren up` prints the install line when it is missing) and about 30 GB of free disk for the base VM. Linux instances on a Mac run through [smolvm](docs/linux-on-mac.md) or Docker Desktop if you have either.
- **Linux**: Docker 24 or newer your user can talk to; gVisor (`scripts/install-gvisor.sh`) so instances do not share the host kernel; for Firecracker `vm` instances, `/dev/kvm`, root and the binaries from `scripts/fetch-firecracker-assets.sh`.

Every method is spelled out in [docs/install.md](docs/install.md), including the system service and removal.

## 2. Run

```sh
warren up                            # start the daemon, create your workspace key, print the URLs
warren mac init --with-agents        # macOS only, once: build the base macOS VM (20 to 40 minutes, mostly downloads)
warren connect claude                # register the MCP server with Claude Code (codex, cursor, windsurf, caret, opencode, gemini too)
warren run -- claude                 # create an instance bound to this directory and run Claude Code inside it
warren run --type mac -- claude      # the same, inside a macOS VM
```

`warren up` picks the runtimes it finds (Docker or smolvm for Linux instances, Lume for macOS ones), installs a launchd agent or a systemd user unit so the daemon survives reboots, and leaves the dashboard at `http://warren.localhost:7337/`. `warren status`, `warren doctor` and `warren down` manage it afterwards.

## 3. Drive an instance

```sh
warren instances create --template warren-base --wait        # a Linux computer (Debian 12, Node, Python, git); --type vm for a Firecracker microVM
warren instances create --template warren-macos --wait       # a macOS VM (type mac is implied)
warren instances exec <id> -- uname -a                       # run a command, get exit code and output
warren shell <id>                                            # an interactive terminal inside
warren files put <id> ./notes.md notes/notes.md              # files in the persistent home
warren instances url <id> --port 8000                        # a signed URL for anything listening inside
warren screenshot <id> -o shot.png                           # macOS: a PNG of the desktop
warren input <id> key cmd+space                              # macOS: keys, clicks, typing and scrolling
warren instances sleep <id>; warren instances start <id>     # mac and vm keep memory and processes across sleep
warren instances fork <id> --name copy                       # copy an instance with its disk (instant on macOS)
```

From an agent, the same calls are MCP tools (`create_instance`, `exec`, `screenshot`, `click`, ...) or plain HTTP ([docs/API.md](docs/API.md)); the Rust, Python and TypeScript clients wrap them. [docs/quickstart.md](docs/quickstart.md) walks through all of this with the output you should see; [docs/run.md](docs/run.md) covers `warren run`.

## What you get

| Feature | What it means |
|---|---|
| Persistent computers | Linux: Debian 12 with Node 24, Python 3, git and build tools; `/home/agent` persists across sleep, stop, restart, resize and template updates. macOS: a full macOS guest whose disk is the home. |
| Real macOS instances | A macOS VM per instance on an Apple silicon Mac (`warren-macos` template, type `mac`): Xcode, the Simulator, Safari, anything that needs macOS. Same API, URLs, gateway and crons as every other instance. Two run at a time per Mac (Apple's limit, enforced before anything is cloned); sleeping frees a slot. |
| A screen for agents | `GET /v1/instances/{id}/screenshot` returns a PNG of the macOS desktop; `POST /v1/instances/{id}/input` clicks, moves, scrolls, types and presses key chords. Driven over the VM's display, so no permission prompts inside the guest. |
| Sleep with memory | macOS instances suspend in 3 to 4 s and resume in 5 to 11 s with every process intact. Firecracker `vm` instances snapshot their memory on sleep and resume it in about 0.2 s. gVisor `default` instances keep their disk; with `--docker-checkpoints` on a Docker 27 engine they keep their processes too ([docs/self-hosting.md](docs/self-hosting.md#sleep-with-memory-on-default-instances)). |
| Fast creates | A `vm` instance is running about 3 s after the request once its image's root filesystem is cached; a `mac` instance in 19 to 23 s. |
| Linux sandboxes on a Mac | Without Docker Desktop: the [smolvm engine](docs/linux-on-mac.md) runs each Linux instance as its own microVM on Apple silicon; Docker Desktop works too. |
| Fork, backup, restore, resize | Copy an instance with its disk, take backups without stopping, restore, grow the disk or change the shape. On macOS forks and backups are APFS clones. |
| One URL per instance | `https://<id>.<base>/` reaches the default port, `https://<id>-<port>.<base>/` any port. WebSockets pass through; signed URLs let browsers in without the key. |
| Two planes, one key | The same `sk_live_...` key drives the Sandbox API (`api.<base>/v1`) and every instance URL; nothing inside an instance ever sees it. |
| Auto-sleep and wake on request | Idle instances sleep; the first request to any of their URLs wakes them and is held until the gateway answers. |
| Agent API and crons | `POST /v1/responses` with sessions, SSE streaming, cancel and replay, on the built-in agent (any OpenAI-compatible model), OpenCode or your own command. Crons wake an instance and run a turn on a schedule. |
| Files and exec | List, read, write, move, delete and download the home over HTTP; run commands with captured exit code, stdout and stderr. |
| Templates | Built-in Linux and macOS images plus your own: register any Linux image and Warren injects its static gateway. Revisions, `name@rev` pins, in-place updates. |
| Egress allowlist and secret broker | A per-instance domain allowlist enforced by a host proxy, and provider keys that stay on the host and are injected on the way out, so an agent can use a key it can never read. See [docs/security-model.md](docs/security-model.md). |
| Clients | `warren` CLI, MCP server, Rust SDK, one-file Python and TypeScript clients, a dashboard. |

## Instance types

| | `default` | `vm` | `mac` |
|---|---|---|---|
| Isolation | Container under gVisor (`runsc`); `runc` fallback for development | Firecracker microVM with its own kernel, optionally through Firecracker's jailer | macOS VM (Virtualization.framework through Lume); at most two running per host |
| Host | Linux x86_64 with Docker; a Mac with smolvm or Docker Desktop | Linux x86_64, root, `/dev/kvm`, a Firecracker binary and a kernel | Apple silicon Mac, macOS 14+, Lume 0.7.2+, a base VM from `warren mac init` |
| Disk | 2 GB to the shape's maximum (12 / 20 / 40 / 80 GB) | 2 to 100 GB | The base image's size up to 500 GB, grow only |
| Sleep | Stop and keep the disk; checkpoint the processes with `--docker-checkpoints` (Docker 27, experimental mode) | Pause and snapshot the memory; processes and pids survive | Suspend; memory and processes survive |
| Screen | Browser terminal (`ttyd`) | Browser terminal | Screenshot and input API |

Shapes are `2/4`, `4/8`, `4/16` and `4/32` vCPU/GB; `mac` needs at least 4 GB. Details in [docs/instances.md](docs/instances.md), [docs/vm-instances.md](docs/vm-instances.md), [docs/macos.md](docs/macos.md) and [docs/limits.md](docs/limits.md).

## How it compares

Claude Code's `/sandbox` wraps the Bash tool on your own machine; Docker Sandboxes gives Claude a Linux microVM on a Mac; E2B, Daytona and Modal rent Linux sandboxes by the hour; Daytona rents Macs by the day. Warren is the one that runs Linux and macOS instances on hardware you own behind one API, with a screen for the macOS ones. It is also newer, single node, and its Linux starts are slower than the clouds'. The full table, with sources and the list of where Warren is behind: [docs/compare.md](docs/compare.md).

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
|  warren-broker      egress allowlist proxy and write-only secrets                |
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

- **CLI**: `warren up`, `warren connect claude`, `warren run -- <command>`, `warren instances ...`, `warren shell <id>`, `warren screenshot <id>`, `warren input <id> ...`, `warren chat <id> "hello"`.
- **MCP server** for Claude Code, Codex, Cursor, Claude Desktop and Windsurf: [mcp/README.md](mcp/README.md). Caret workspaces target a template by name (`warren.template: warren-macos`).
- **Rust SDK** (`warren-sdk`), one-file **Python** and **TypeScript** clients ([sdk/python](sdk/python/README.md), [sdk/typescript](sdk/typescript/README.md)), and a **dashboard** at the root host.
- **For coding agents**: a prompt to paste in [docs/for-agents.md](docs/for-agents.md), a Claude Code skill (`warren skill install`) and an index in [docs/llms.txt](docs/llms.txt).

## Status

- **Version 0.2.0.** Binaries for Apple silicon Macs and x86_64 Linux are on the [releases page](https://github.com/veerareddyvishal144/warren/releases); the Homebrew tap and the install script fetch them. The remaining launch work is tracked in [docs/launch-checklist.md](docs/launch-checklist.md).
- **Single node.** One daemon drives one host. Multi-node placement is roadmap.
- **Linux `default` type** is exercised end to end in CI under Docker. **`vm` (Firecracker)** boots, sleeps with its memory, wakes from the snapshot and runs through the jailer in the end-to-end suite on one KVM host.
- **macOS is verified on one Mac**: a MacBook Pro (Apple silicon, 16 GiB) running `scripts/mac/e2e.sh` on 2026-11-22: create, exec, screenshot, input, sleep and wake with processes intact, daemon restart with the VM adopted, fork, guest isolation, host capacity, delete. The numbers and their method are in [docs/benchmarks.md](docs/benchmarks.md). It has not run on a second Mac, a Mac mini, or any other macOS host version.
- **Not yet exercised beyond the development machines**: `warren up`, `warren connect` and `warren run`; the egress allowlist and secret broker; the install path; the screen API. Treat them as fresh.
- **Memory checkpoints for gVisor (`default`) instances** need `--docker-checkpoints` and a Docker 27.x engine in experimental mode; Docker 28 and later cannot restore them yet ([docs/self-hosting.md](docs/self-hosting.md#sleep-with-memory-on-default-instances)).
- **Not done**: a live view of the macOS screen through the edge, host-side egress enforcement for macOS guests (the guest applies its own policy with `pf`), TLS in the daemon (use a proxy), an audit log. The complete list is [docs/limits.md](docs/limits.md).

## Documentation

[docs/index.md](docs/index.md) lists every page. Start with [install](docs/install.md), [quickstart](docs/quickstart.md), [run](docs/run.md) and [concepts](docs/concepts.md); then [instances](docs/instances.md), [templates](docs/templates.md), [macOS](docs/macos.md), [security model](docs/security-model.md), [limits](docs/limits.md) and [self-hosting](docs/self-hosting.md). The wire contract is [docs/API.md](docs/API.md); development rules are in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## Roadmap

- **Live view of macOS instances**: the screenshot and input API exists; next is a streamed view through the edge (noVNC) for a human watching the agent.
- **Memory checkpoints for `default` without going through Docker**: `--docker-checkpoints` keeps processes across sleep on Docker 27.x with gVisor but needs experimental mode, and Docker 28 and later cannot restore (moby/moby#50750). Driving `runsc checkpoint` and `runsc restore` directly lifts both requirements.
- **Linux guests on Mac hosts**, outside the two-guest limit, as cheap extra capacity on the same machine.
- **Harden the Mac host**: host-side `pf` egress drops for the NAT subnet (today each guest enforces its own policy), one `lume serve` per instance, and in time a runtime on Virtualization.framework without Lume.
- **Secrets at rest**: encrypt instance env and brokered secrets with a key kept outside the data directory.
- **Multi-node scheduler**: place instances across hosts; the `region` field is already on every instance.
- **Cloudflare edge**: terminate TLS and authenticate at the edge, with the daemon as origin.

## License

Apache-2.0. See [LICENSE](LICENSE).
