# Changelog

All notable changes to Warren are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- The `mac` instance type: a macOS virtual machine per instance on an Apple silicon host, through Lume's API server. New crate `warren-runtime-mac` (clone of a base VM per instance, headless start with a read-only shared folder carrying the gateway and its environment, sleep as suspend with memory kept, host capacity of two macOS guests reported as `409 host_capacity_reached`, copy-on-write backups), new crate `warren-gateway-client` shared with the Firecracker runtime, daemon flags `--runtime none`, `--mac-base-vm`, `--lume-api`, `--lume-bin`, `--mac-vnc`, the built-in template `warren-macos`, `GET /v1/health` reporting `lume`, dashboard/CLI/MCP/SDK support for type `mac`, and the Mac-side scripts `scripts/mac/{build-base.sh,e2e.sh,guest/}`. `docs/macos.md` is the operator guide.
- `warren-gateway`: `GET /v1/stats` (guest-side CPU, memory and disk) and user names from the system user database (Open Directory on macOS, NSS on Linux) when `/etc/passwd` lacks the entry.
- `Runtime::sleep`: runtimes may keep memory across sleep; the lifecycle calls it instead of `stop`. A `host_capacity_reached` refusal on start puts the instance back to `stopped`/`sleeping` instead of `failed`.

### Changed

- `warren-gateway` now builds on non-Linux hosts (macOS included): the PID 1 code in `init.rs` is compiled only on Linux and other targets get a stub, so `--init` and `POST /v1/shutdown` report `not_supported` there. Groundwork for macOS sandboxes.

- `docs/macos.md`: Phase 0 feasibility results for macOS sandboxes (Lume on Apple silicon) and the Phase 1 design for a `mac` instance type. `scripts/mac/boot-test.sh` measures boot-to-SSH and VM memory.

### Fixed

- `warren-gateway` read its effective uid and gid from `/proc/self/status`, which does not exist on macOS, so `/v1/health` reported `home_owner` as 4294967295 there. It now uses `geteuid`/`getegid` (nix `user` feature).

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
