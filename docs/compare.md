# How Warren compares

Where Warren sits next to the other ways of giving a coding agent a computer that is not the one you are typing on. Facts about other products come from their public documentation as read on 2026-11-19 (sources at the end); their prices are theirs and change without notice. Warren has no price: it is Apache-2.0 software you run on hardware you already own.

The short version: Warren is the only entry below that puts **Linux containers, Linux microVMs and real macOS VMs behind one API on your own hardware**. The others are host-local (Claude Code's `/sandbox`, Docker Sandboxes), Linux-only in someone else's cloud (E2B, Daytona, Modal, Vercel), a cloud Mac on a 24-hour lease (Daytona macOS), or a VM manager you script yourself (Tart, Lume). The second half of this page is about where each of them is ahead.

## The table

Two tables with the same rows, so they fit on a screen.

### Isolation, OS, API, lifecycle

| | Isolation boundary | OS inside | Where it runs | API: create, exec, files | Sleep with memory, fork, backups |
|---|---|---|---|---|---|
| **Warren** | gVisor container (`default`), Firecracker microVM (`vm`), macOS VM on Virtualization.framework through Lume (`mac`) | Linux (Debian 12 built in; any Linux image) and macOS | Hardware you own: a Linux host, an Apple silicon Mac, or both | REST for all three (`POST /v1/instances`, `/exec`, the files API) plus an Agent API inside each instance, a CLI, an MCP server and Rust, Python and TypeScript clients | `mac` and `vm`: sleep keeps memory and processes (Lume suspend, Firecracker snapshots). `default`: keeps them too with `--docker-checkpoints` on Docker 27.x in experimental mode (gVisor checkpoint/restore; Docker 28+ cannot restore, moby/moby#50750), else the disk only. Fork and backups on every type |
| Claude Code `/sandbox` | OS sandbox on the host (Seatbelt on macOS) around the **Bash tool only**; MCP servers, hooks and file edits run unsandboxed | The host's own OS | Your machine, inside the Claude Code process | None: a setting of the CLI, not a service | None; there is no instance to sleep, fork or back up |
| Docker Sandboxes (`sbx run claude`) | Linux microVM per session | Linux only | Your Mac (macOS 14+, Apple silicon) through Docker Desktop | CLI-driven; the docs describe no create/exec/files API for other programs to call | Not documented |
| E2B, Daytona, Modal, Vercel Sandbox (cloud Linux) | microVM or gVisor, provider-dependent | Linux only | The provider's cloud | Full SDKs and REST: create, exec, files, exposed ports | Varies: pause/resume and snapshots on some; check each provider |
| Daytona macOS sandboxes | A Mac reserved for you (Apple's licence requires 24-hour exclusive leases for hosting) | macOS | Daytona's cloud, us-east only at launch | Daytona's SDK and API, per the docs | Not documented at launch |
| DIY: Tart or Lume on a Mac | Virtualization.framework VM | macOS or Linux guests | Your Mac | VM-level only: create, clone, run, stop from a CLI (Lume also has a local HTTP API). Exec and files are yours to build over SSH | Clone (APFS, instant) on both; suspend through Lume's API; backups are copies you script |

### Reachability, credentials, screen, price, maturity

| | URL per instance | Credential proxy and egress allowlist | Screen / GUI | Price | Maturity |
|---|---|---|---|---|---|
| **Warren** | Yes: `https://<id>.<base>/` and `https://<id>-<port>.<base>/` for any port, signed URLs for browsers, WebSockets through | **New in this release**: a per-instance domain allowlist enforced by a host proxy, and a secret broker that keeps provider keys out of the instance and injects them on the way out ([security-model.md](security-model.md)). Not yet exercised outside the development machines. Before it: `allow-all` / `deny-all` plus host-wide blocks of the metadata and private ranges on Linux | Linux: browser terminal (`ttyd`). macOS: screenshots (PNG) and pointer and keyboard input through the VM's VNC server (`GET .../screenshot`, `POST .../input`; MCP tools `screenshot`, `click`, `type_text`, `press_key`, `scroll`), so an agent can see and drive Safari, Xcode or the Simulator; no live VNC view through the edge yet | No price; Apache-2.0. You pay for the hardware and the power | 0.1.0, unreleased; single node. `default` is tested in CI end to end; `vm` is experimental and tested on one machine; `mac` is Phase 1, verified by an end-to-end script on one MacBook Pro |
| Claude Code `/sandbox` | No | Filesystem and network rules for the Bash tool (network through a local proxy with allowed domains, per Anthropic's docs); no credential proxy. Anthropic says it is not sufficient for unattended runs | The host's own screen | Included with Claude Code | Shipping; Anthropic reports 84% fewer permission prompts with it on |
| Docker Sandboxes | Not documented | **Yes**: a host-side credential proxy and an egress policy, per Docker's docs | No | Part of Docker Desktop; check Docker's terms for your organisation | Shipping in Docker Desktop; we have not used it beyond reading the docs |
| E2B, Daytona, Modal, Vercel (cloud Linux) | Yes, typically per exposed port | Egress controls on some providers; credentials are normally passed in as environment variables | Desktop variants on some providers | Roughly $0.07 to $0.17 per sandbox-hour, with measured starts of 0.7 to 3 s (third-party comparison, see sources) | Production services with paying customers |
| Daytona macOS | Per the docs | Per the docs; not verified by us | Not verified by us | About $60 per day, billed as 24-hour Mac reservations | Launched 2026-11-17 |
| DIY Tart / Lume | No | No | VNC | Lume is MIT. Tart (now owned by OpenAI) has its own licence terms; read them | Tart is established CI tooling; Lume is 0.x. Wrappers such as ClodPod, Chamber and MacVisor are small projects or tech previews |

## Where Warren is behind

In plain terms, as of this release:

- **Slower starts than cloud Linux sandboxes.** A `mac` instance takes 19 to 23 s to be usable and 5 to 11 s to wake; a `vm` instance takes about 3 s once its root filesystem is cached (18 s the first time per image); E2B, Modal and the others start Linux sandboxes in 0.7 to 3 s. Warren's `default` start time is not measured yet ([benchmarks.md](benchmarks.md)).
- **`default` sleep keeps processes only with a flag, and only on Docker 27.** `mac` and `vm` keep memory across sleep as they are. The gVisor container type has `--docker-checkpoints` (Docker's checkpoint API, experimental mode required): on Docker 27.x with runsc it checkpoints in 0.2 s and restores in 0.5 s with pids intact; on Docker 28 and later the engine's restore is broken ([moby/moby#50750](https://github.com/moby/moby/issues/50750)), the wake boots cold, and the daemon says so in its log ([self-hosting.md](self-hosting.md#sleep-with-memory-on-default-instances)). Providers with pause/resume keep the process tree on Linux without a setting. Warren's `vm` type resumes in 0.2 s on a KVM host; the `default` figures are in [benchmarks.md](benchmarks.md).
- **The credential proxy and egress allowlist are new and unproven.** Docker Sandboxes has shipped both; Claude Code has a domain allowlist for Bash. Warren's arrived with this release and has been exercised on the development machines only. Until you have verified it on your host, treat `allow-all` and `deny-all` as the policies that are known to work.
- **Two macOS guests per Mac, and no cloud.** Daytona rents you as many Macs as you pay for, in their region, with an SLA. Warren runs on the Mac you have: two guests, your uptime, no regions, no elasticity. More capacity means more Macs and, until multi-node lands, one daemon per Mac.
- **A 0.x release, single node, one maintainer.** Binaries exist for two platforms and the macOS path has run on one Mac; there is no community to speak of yet. The cloud providers are companies with support contracts.
- **Depends on Lume**, which hosts every VM in one process and exposes an unauthenticated local API ([limits.md](limits.md)). The DIY route has the same dependency with the same properties; the cloud providers hide theirs.
- **Egress policy on macOS instances is enforced inside the guest** by `pf` rules the gateway launcher loads from the daemon's policy (base images from this release), not by the host, and the daemon installs no host-wide egress drops on a Mac. On Linux the host enforces `deny-all` and `allowlist`.
- **No TLS built in, no audit log, no per-workspace CPU/RAM totals**, no disk quota on the Linux home directory. The cloud providers have all of these because they must.
- **The three-command path is new.** `warren up`, `warren connect claude` and `warren run -- claude` were added in this release ([run.md](run.md)); `sbx run claude` and `/sandbox` have had more hands on them.

## Where Warren is ahead

- One API, one CLI, one MCP server for a gVisor container, a Firecracker microVM and a macOS VM; the client does not care which it got.
- Real macOS on a Mac you already own, with sleep that keeps memory and processes (3 to 4 s down, 5 to 11 s back) and instant APFS forks and backups; no 24-hour lease, no per-hour meter, nothing leaves your hardware.
- A screen for macOS instances without touching the guest: a PNG screenshot and click, type, key and scroll actions over the VM's VNC server, exposed as API routes, CLI commands and MCP tools, so a coding agent can drive Xcode, Safari or the Simulator and see the result. The DIY route gives you a VNC window to look at yourself; the cloud Linux providers' desktop variants do not run macOS.
- A URL for every instance and every port, with signed URLs for browsers, so a preview or a terminal is a link.
- An Agent API inside each instance, crons that wake it, and a files API, so an instance is a destination for work and not only a shell.
- Everything, including the gateway inside the instance, is Apache-2.0 Rust you can read.

## Sources

- Claude Code sandboxing: [code.claude.com/docs/en/sandbox-environments](https://code.claude.com/docs/en/sandbox-environments) and [anthropic.com/engineering/claude-code-sandboxing](https://www.anthropic.com/engineering/claude-code-sandboxing) (the 84% figure).
- Docker Sandboxes: [docs.docker.com/ai/sandboxes](https://docs.docker.com/ai/sandboxes/).
- Cloud Linux sandboxes (prices and start times): [LogRocket, "Comparing AI agent sandbox platforms: E2B, Modal, Daytona and more"](https://blog.logrocket.com/comparing-ai-agent-sandbox-platforms-e2b-modal-daytona-and-more/).
- Daytona macOS sandboxes: [daytona.com/docs/macos-sandboxes](https://www.daytona.com/docs/macos-sandboxes).
- Apple's licence terms for virtual macOS: [macOS Tahoe SLA](https://www.apple.com/legal/sla/docs/macOSTahoe.pdf).
- Tart: [github.com/cirruslabs/tart](https://github.com/cirruslabs/tart). Lume: [github.com/trycua/cua](https://github.com/trycua/cua).
- Warren's own numbers: [benchmarks.md](benchmarks.md), measured on 2026-11-19.

## smolvm as an engine under Warren

smolvm is not only a competitor on the table above: its Docker Engine API mode
(`smolvm engine`) lets Warren's existing Docker runtime run Linux instances
as libkrun microVMs on a Mac with no Docker Desktop. Warren adds what smolvm
leaves to the caller: the API, persistence across stop and start, URLs, the
egress broker, workspaces and keys, and the agent integrations. Measured on
this Mac: a `warren-base` instance created and running in 8 s through the
engine, exec, sleep, wake and delete through the normal API. Memory
checkpoints and live branching are not reachable through the Docker API and
would need a native smolvm runtime; see [linux-on-mac.md](linux-on-mac.md).
