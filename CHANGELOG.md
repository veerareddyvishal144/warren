# Changelog

All notable changes to Warren are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - Unreleased

The first release: a single-node platform in one Rust binary.

### Added

- `warrend`: Sandbox API (`/v1`), edge proxy with host-header dispatch, lifecycle, per-minute metering and cron in one process; configuration from flags and `WARREN_*` variables; graceful shutdown; reconciler that adopts running containers at startup.
- Workspaces with API keys (`sk_live_...`, SHA-256 at rest), open signup with a configurable welcome credit, wallet, ledger and dev top-ups.
- Instances: create, get, list, patch, stop, start, sleep, restart, resize, update, fork, delete; exec with captured output; logs; per-minute metrics; backups and restore; signed URLs.
- Auto-sleep after an idle timeout and wake on request at any instance URL, exec or cron firing; sleeping and stopped instances bill disk only.
- Metering from monthly list prices ($0.80 per vCPU, $0.70 per GB RAM, $0.09 per GB disk; `vm` compute x 2.5) with whole-minute charges and fractional carry; the one-day-of-balance rule on create; exhausted wallets stop running instances; awake limits that grow with cumulative top-ups.
- Templates: built-in `warren-base`, `warren-agent` and `warren-opencode`; workspace templates from any image with revisions and `name@rev` pins; in-place updates that keep the persistent disk.
- Edge: workspace-key, signed-URL and cookie authentication on instance hosts, preview URLs for any port, WebSocket passthrough, CORS.
- `warren-gateway` inside every instance: Agent API (`POST /v1/responses` with sessions, SSE streaming, replay and cancel), files API confined to the home directory, exec with a per-instance token, health, `ttyd` supervision on port 7681, PID 1 duties in microVMs; `echo`, `openai` and `command` drivers.
- Runtimes: Docker with gVisor (`runsc`) and a `runc` fallback; experimental Firecracker microVMs (`vm` type) behind `--firecracker`.
- Crons: scheduled prompts for agent instances with five-field schedules and IANA timezones.
- Dashboard: key login, instances, lifecycle actions, terminal via signed URL, exec console, chat, wallet and ledger, dev top-up.
- Clients: `warren` CLI, MCP server (`warren-mcp`), Rust SDK, one-file Python and TypeScript clients.
- Scripts: `install-gvisor.sh`, `build-images.sh`, `dev-up.sh`, `e2e.sh`, `fetch-firecracker-assets.sh`; Makefile targets for building, testing, images and end-to-end runs.
- Documentation: README, quickstart, concepts, instances, templates, billing, URLs, Agent API, files, exec, crons, VM instances, self-hosting, for-agents, security, `llms.txt`; CI with fmt, clippy, unit tests, image builds and end-to-end tests; Dependabot for Cargo and GitHub Actions.

[Unreleased]: https://github.com/veerareddyvishal144/warren/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/veerareddyvishal144/warren/releases/tag/v0.1.0
