# Warren documentation

Warren gives every agent or end user a persistent Linux computer reachable over HTTPS, with auto-sleep, prepaid metering and an Agent API. One daemon (`warrend`) runs the platform on a single host; one gateway (`warren-gateway`) runs inside every instance.

## Start here

| Page | What it covers |
|---|---|
| [Quickstart](quickstart.md) | Install, build the images, start the daemon, sign up, create an instance, exec, chat, open the terminal. |
| [Concepts](concepts.md) | Instance, template, session and response, workspace, wallet, budget; two planes one key; derivable URLs. |
| [For coding agents](for-agents.md) | A prompt to paste into a coding agent, and how to feed it `llms.txt`. |

## Using instances

| Page | What it covers |
|---|---|
| [Instances](instances.md) | Lifecycle states, every action with curl, auto-sleep in this build, shapes and prices, resize, fork, update, backups. |
| [Templates](templates.md) | The built-in catalog, registering your own image, revisions and pins, the image contract. |
| [URLs](urls.md) | Instance and preview URLs, API keys on instance hosts, signed URLs, the session cookie, CORS. |
| [Exec](exec.md) | Running shell commands with captured output, users, timeouts, limits. |
| [Files](files.md) | The files API: list, read, write, move, delete, download a directory as a tarball. |
| [Agent API](agent-api.md) | `POST /v1/responses`, streaming events, sessions, models, drivers. |
| [Crons](crons.md) | Scheduled prompts for agent instances. |
| [VM instances](vm-instances.md) | The experimental Firecracker type: requirements, assets, flags, limits. |

## Operating Warren

| Page | What it covers |
|---|---|
| [Billing](billing.md) | Rates, the per-minute meter, the one-day rule, awake limits, top-ups, the ledger. |
| [Self-hosting](self-hosting.md) | A single node on Debian or Ubuntu: Docker, gVisor, the data directory, systemd, Cloudflare or Caddy with a wildcard certificate, environment variables, backups. |
| [Security](security.md) | Threat model: edge auth, gVisor, per-instance token, files confinement, what is not isolated yet. |

## Reference

| Page | What it covers |
|---|---|
| [API contract](API.md) | The wire contract every component implements: Sandbox API, Agent API, edge rules. |
| [Design](DESIGN.md) | Crates, storage layout, metering rules, sleep and wake, security model, testing strategy. |
| [Development](DEVELOPMENT.md) | Toolchain, environment, how to work on one crate, conventions. |
| [Testing](TESTING.md) | Unit tests, the end-to-end suite, the vm scenario, and a manual curl walkthrough of every feature. |
| [llms.txt](llms.txt) | A one-line index of every page for language models. |

Also in the repository: [images/common/README.md](../images/common/README.md) (the template image contract), [mcp/README.md](../mcp/README.md) (the MCP server), [sdk/python/README.md](../sdk/python/README.md) and [sdk/typescript/README.md](../sdk/typescript/README.md) (one-file clients), [CONTRIBUTING.md](../CONTRIBUTING.md), [SECURITY.md](../SECURITY.md), [CHANGELOG.md](../CHANGELOG.md).
