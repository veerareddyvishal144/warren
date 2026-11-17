# Changelog

All notable changes to Warren are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.1.0] - Unreleased

The first release: a single-node platform in one Rust binary, free software you host yourself.

### Added

- `warrend`: Sandbox API (`/v1`), edge proxy with host-header dispatch, lifecycle and cron in one process; configuration from flags and `WARREN_*` variables; graceful shutdown; reconciler that adopts running containers at startup.
- Workspaces with API keys (`sk_live_...`, SHA-256 at rest) and open signup (`POST /v1/signup` -> `{ "workspace", "api_key", "key_id" }`), switchable with `--signup-enabled`.
- Instances: create, get, list, patch, stop, start, sleep, restart, resize, update, fork, delete; exec with captured output; logs; per-minute metrics; backups and restore; signed URLs.
- Auto-sleep after an idle timeout and wake on request at any instance URL, exec or cron firing; sleeping and stopped instances keep only their disk.
- Awake quota: `--max-awake-instances N` (`WARREN_MAX_AWAKE_INSTANCES`, unset means unlimited) caps the instances a workspace keeps awake at once; enforced on create, start, wake and fork with `409 instance_limit_reached`; `GET /v1/workspace` reports `awake_limit` (number or `null`) and `awake_count`.
- Templates: built-in `warren-base`, `warren-agent` and `warren-opencode`; workspace templates from any image with revisions and `name@rev` pins; in-place updates that keep the persistent disk.
- Edge: workspace-key, signed-URL and cookie authentication on instance hosts, preview URLs for any port, WebSocket passthrough, CORS.
- `warren-gateway` inside every instance: Agent API (`POST /v1/responses` with sessions, SSE streaming, replay and cancel), files API confined to the home directory, exec with a per-instance token, health, `ttyd` supervision on port 7681, PID 1 duties in microVMs; `echo`, `openai` and `command` drivers.
- Runtimes: Docker with gVisor (`runsc`) and a `runc` fallback; experimental Firecracker microVMs (`vm` type) behind `--firecracker`.
- Crons: scheduled prompts for agent instances with five-field schedules and IANA timezones.
- Dashboard: key login, instances, lifecycle actions, terminal via signed URL, exec console, chat, workspace quota.
- Clients: `warren` CLI, MCP server (`warren-mcp`), Rust SDK, one-file Python and TypeScript clients.
- Scripts: `install-gvisor.sh`, `build-images.sh`, `dev-up.sh`, `e2e.sh`, `fetch-firecracker-assets.sh`; Makefile targets for building, testing, images and end-to-end runs.
- Documentation: README, quickstart, concepts, instances, templates, quotas, URLs, Agent API, files, exec, crons, VM instances, self-hosting, for-agents, security, `llms.txt`; CI with fmt, clippy, unit tests, image builds and end-to-end tests; Dependabot for Cargo and GitHub Actions.

### Security

- Tenant network isolation: `warren-net` and `warren-internal` are created with inter-container communication disabled, so instances cannot reach each other over the bridge; Firecracker taps use bridge port isolation. Operators upgrading from an earlier build must remove the old networks once: stop instances, `docker network rm warren-net warren-internal`, restart `warrend`.
- Egress policy: when `warrend` runs as root it installs iptables rules in the `DOCKER-USER` chain (and `FORWARD` rules for the Firecracker bridge) that drop traffic from instances to `169.254.0.0/16` (cloud metadata) always, and to the RFC 1918 ranges and the host's own addresses unless `--allow-private-egress` is set. When not root it logs a warning and installs nothing.
- Gateway authentication: every gateway route except `GET /v1/health` and `/v1/version` requires the per-instance token. The edge and the cron scheduler add it to the requests they forward, so clients never see or send it. The token is no longer inherited by exec shells, agent tool shells or the terminal.
- Edge hardening: a workspace API key sent as `Authorization: Bearer` to an instance URL is no longer forwarded into the instance; CORS on instance hosts allows only the dashboard origin and the instance's own origins; cookie (signed URL) sessions are accepted only for same-origin requests; the signed-URL redirect uses an absolute `Location`; API requests have a 60 s timeout and the listener a global concurrency limit.
- Signup: `--signup-enabled` (default `true`) and a per-IP limit of 10 signups per hour (loopback exempt in `--dev`). Hosted deployments should turn open signup off or put it behind their own flow, and set `--max-awake-instances` so one workspace cannot take the whole host.

### Known limitations

- The base domain should be registered on the Public Suffix List, or each tenant given its own domain, so that instance subdomains are cross-site to each other; browsers otherwise treat `<id>.<base-domain>` hosts as same-site.
- No per-instance egress allowlist; only `allow-all`, `deny-all` and the host-wide block above.
- The awake quota is the only per-workspace limit; CPU, RAM and disk totals per workspace are not enforced, and the bind-mounted home has no filesystem quota.
- The `agent` user has passwordless sudo inside its own instance, and `exec` as `root` is allowed, both by design; there is no privilege boundary inside an instance.
- See [docs/security.md](docs/security.md) for the full list (`runc` fallback, disk quotas, TLS, bearer URLs, side channels, audit log).

[Unreleased]: https://github.com/veerareddyvishal144/warren/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/veerareddyvishal144/warren/releases/tag/v0.1.0
