# Warren documentation

Warren gives every agent, or every end user of your product, a persistent computer reachable over HTTPS: a Linux container under gVisor, a Linux microVM under Firecracker, or a macOS virtual machine on an Apple silicon Mac, all behind one API with auto-sleep, a files API, exec and an Agent API. One daemon (`warrend`) runs the platform on a single host; one gateway (`warren-gateway`) runs inside every instance.

Warren is free software you host yourself, Apache-2.0, with no billing and no metering of any kind; the API exposes each instance's status, resources and uptime (`started_at`, `last_activity_at`, per-minute `metrics`) for operators who need them.

## Start here

| Page | What it covers |
|---|---|
| [Install](install.md) | Getting the `warren` binary and its runtime on a Mac or a Linux host. |
| [Quickstart](quickstart.md) | `warren up`, the first instance, exec, files, the terminal, sleep and wake. |
| [Run](run.md) | `warren run -- <command>`: a coding agent (Claude Code, Codex) inside a fresh instance; `warren connect` for MCP clients. |
| [Concepts](concepts.md) | Instance, template, session and response, workspace, awake quota; two planes one key; derivable URLs. |
| [For coding agents](for-agents.md) | A prompt to paste into a coding agent, and how to feed it `llms.txt`. |
| [How it compares](compare.md) | Warren next to Claude Code's `/sandbox`, Docker Sandboxes, the cloud Linux sandboxes, Daytona macOS and DIY Tart or Lume, with sources, and where Warren is behind. |

## Using instances

| Page | What it covers |
|---|---|
| [Instances](instances.md) | Lifecycle states, every action with curl, auto-sleep in this build, shapes, resize, fork, update, backups. |
| [Templates](templates.md) | The built-in catalog (Linux and macOS), registering your own image, revisions and pins, the image contract. |
| [URLs](urls.md) | Instance and preview URLs, API keys on instance hosts, signed URLs, the session cookie, CORS. |
| [Exec](exec.md) | Running shell commands with captured output, users, timeouts, limits. |
| [Files](files.md) | The files API: list, read, write, move, delete, download a directory as a tarball. |
| [Agent API](agent-api.md) | `POST /v1/responses`, streaming events, sessions, models, drivers. |
| [Crons](crons.md) | Scheduled prompts for agent instances. |
| [VM instances](vm-instances.md) | The experimental Firecracker type: requirements, assets, flags, limits. |
| [macOS sandboxes](macos.md) | The `mac` type: macOS virtual machines through Lume on an Apple silicon host; requirements, base image, the two-VM limit, daemon flags, design. |

## Operating Warren

| Page | What it covers |
|---|---|
| [Quotas](quotas.md) | The awake quota: `--max-awake-instances`, which states count, which actions enforce it, `awake_limit` and `awake_count`, sizing a host. |
| [Self-hosting](self-hosting.md) | A single node on Debian or Ubuntu: Docker, gVisor, the data directory, systemd, Cloudflare or Caddy with a wildcard certificate, environment variables, backups. |
| [Security model](security-model.md) | The map: assets and trust boundaries, what each runtime isolates, what is not isolated today, the new egress allowlist and secret broker, known gaps and roadmap. |
| [Security](security.md) | The detailed reference: edge authentication, gVisor, the per-instance token, files confinement, networking and egress rules, signup, every known limitation. |
| [Limits](limits.md) | Hard limits by type: two macOS guests per host, 4 GB minimum, no nested virtualization, grow-only disks, no screen or terminal in macOS guests, the Lume dependency, licence posture, gVisor and Firecracker limits. |
| [Benchmarks](benchmarks.md) | Measured numbers (create, sleep, wake, fork, delete, memory) with the machine and method, what is not measured yet, and what to measure next. |

## Reference

| Page | What it covers |
|---|---|
| [API contract](API.md) | The wire contract every component implements: Sandbox API, Agent API, edge rules. |
| [Design](DESIGN.md) | Crates, storage layout, the awake quota, sleep and wake, security model, testing strategy. |
| [Development](DEVELOPMENT.md) | Toolchain, environment, how to work on one crate, conventions. |
| [Testing](TESTING.md) | Unit tests, the end-to-end suite, the vm scenario, and a manual curl walkthrough of every feature. |
| [llms.txt](llms.txt) | A one-line index of every page for language models. |

## Project

| Page | What it covers |
|---|---|
| [Launch checklist](launch-checklist.md) | What must be true before the release is announced, the Show HN draft, and FAQ answers. |
| [Changelog](../CHANGELOG.md) | Release notes. |
| [Security policy](../SECURITY.md) | How to report a vulnerability, response targets, scope. |
| [Contributing](../CONTRIBUTING.md) | How to propose changes. |

Also in the repository: [images/common/README.md](../images/common/README.md) (the template image contract), [scripts/mac/README.md](../scripts/mac/README.md) (the Mac-side scripts), [mcp/README.md](../mcp/README.md) (the MCP server), [sdk/python/README.md](../sdk/python/README.md) and [sdk/typescript/README.md](../sdk/typescript/README.md) (one-file clients).
