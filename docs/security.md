# Security

What Warren protects, how, and what it does not protect yet. Read this before putting untrusted code or untrusted users on a host. Vulnerability reports: [SECURITY.md](../SECURITY.md).

## Assets and parties

| Asset | Threatened by |
|---|---|
| The host: its kernel, Docker daemon, other instances, the data directory | Code running inside an instance (agents run arbitrary code by design) |
| A workspace's instances, files, sessions and wallet | Other workspaces; anyone on the network who reaches the edge |
| API keys and the signed-URL secret | Leaks through instances, logs, backups, the dashboard |
| The operator's money and capacity | Abuse of open signup, runaway instances |
| Private networks reachable from the host | Egress from instances |

Trust boundaries: the edge (everything outside is untrusted), the instance boundary (everything inside is untrusted by the host), the workspace boundary (instances and keys of one tenant are invisible to another).

## Edge authentication

Instances never get public IPs and publish no ports; every request passes the edge inside `warrend`, which:

- Classifies the `Host` header (`api`, root, `<id>`, `<id>-<port>`); a label with an extra dot is not an instance host.
- Requires a workspace key (`X-Warren-Key` or `Authorization: Bearer`), a signed URL token or the `warren_session` cookie on every instance request, answering `401 invalid_api_key` otherwise.
- Checks that the instance belongs to the key's workspace and answers `404 not_found` otherwise, so ids of other tenants cannot be probed into existence.
- Strips `X-Warren-Key` and the `warren_session` cookie before forwarding, so the code inside an instance never receives the workspace key or the session token.

Keys are `sk_live_` plus 40 base62 characters (about 238 bits); only their SHA-256 is stored and the plaintext is shown once. Keys can expire (`expires_at`) and be deleted at any time. Signed URL tokens are HMAC-SHA256 over `<id>|<port>|<expires_at>` with a 256-bit secret kept at `<data>/secret` with mode 0600 (or supplied with `--secret`); tokens are bound to one instance and port and expire after at most 24 hours. Instance ids are 10 random lowercase alphanumerics, but authorization never relies on them being secret.

## Isolation of the default type: gVisor

Default instances are Docker containers run with the `runsc` runtime when it is registered (`--runtime auto` or `runsc`). gVisor interposes a user-space kernel between the container and the host kernel: the application's system calls are served by the Sentry, and only a small, audited set reaches the host. This is the boundary that makes running agent-written code on a shared host acceptable. CPU and memory limits are enforced per container from the instance's shape. `runc` remains as a fallback for development; the daemon logs a warning when it falls back, and you should set `WARREN_RUNTIME=runsc` in production so a missing gVisor is a startup error.

The `vm` type puts a hardware virtualisation boundary (Firecracker, KVM) around the instance instead, at the cost of needing root on the host and being experimental.

## The per-instance token

The only secret injected into an instance is `WARREN_INSTANCE_TOKEN`, a random 40-character token generated per instance and known to the daemon alone. The gateway requires it in `X-Warren-Instance-Token` on `POST /v1/exec`, which is how the daemon runs commands in VM instances without Docker. The edge never forwards that header from clients, and nothing else accepts the token, so a leaked token lets an attacker who is already inside the instance run commands in that same instance, and nothing more. The workspace key is never present inside an instance; agent credentials you choose to pass in `env` are, and are visible to anything running there.

## Files confinement

The gateway's files API resolves every path against the home directory and rejects `..`, absolute paths outside it, and symlinks that point outside or dangle, with `403 forbidden`; the home itself cannot be deleted or moved. The daemon's own operations on disks (fork, backup, restore, delete) stay under `<data>/instances/<id>/`. Exec output is capped at 1 MiB per stream and exec commands at 1 MiB, so an instance cannot flood the daemon's memory through those paths; JSON bodies are capped at 4 MiB.

## Secrets at rest

The database holds API key hashes, never plaintext keys. The data directory holds the HMAC secret (0600) and, in instance env records, whatever credentials you gave your instances: treat the data directory and its backups as sensitive. Cron prompts are stored in the database. Logs use `tracing`; keys and tokens are not logged by design, but treat log files as sensitive anyway.

## Networking

Instances with `allow-all` sit on the `warren-net` bridge and can reach the internet; `deny-all` moves them to `warren-internal`, a bridge with no route out. When the daemon runs as root it installs an nftables rule set at startup that blocks instance egress to RFC1918 ranges, so an instance cannot reach the operator's private network or the Docker host's other services; without root this is skipped with a warning. Instances on the same bridge can reach each other's IPs directly, which matters if your tenants are mutually untrusted (see below).

## What is not isolated or protected yet

Be explicit about these when you decide what to run.

- **Instance-to-instance traffic.** Instances on `warren-net` share a bridge and can talk to each other's gateway ports by IP, bypassing the edge. The gateway's non-exec endpoints have no auth of their own, so a hostile instance could read another tenant's files on the same host. Per-instance network isolation is not implemented; until it is, run mutually untrusted tenants on separate hosts or with `deny-all`.
- **`runc` fallback.** Without gVisor the container shares the host kernel with ordinary namespacing only. Fine for your own code, not for untrusted code.
- **Egress allowlists.** Only `allow-all` and `deny-all` exist; an instance that may reach the internet may reach all of it. Allowlist egress is on the roadmap.
- **Disk quotas.** The disk size is a sizing and billing number; the bind-mounted home has no filesystem quota in this build, so an instance can fill the data volume. Watch `disk_used_bytes` and free space on the host.
- **Open signup and no production payments.** `POST /v1/signup` is unauthenticated and grants the free credit; there is no rate limiting on it or on the API in general. On a public host set `WARREN_FREE_CREDIT_MICROS=0` or keep the API host private to your backend, and expect to run a proxy with rate limits in front.
- **TLS.** The daemon speaks plain HTTP; TLS comes from Caddy, Cloudflare or another proxy ([self-hosting.md](self-hosting.md)). Do not expose 7337 directly.
- **Signed URLs and cookies are bearer credentials.** Anyone who gets the URL can use it until it expires; the cookie makes a browser stay authenticated on that host for the token's lifetime. There is no per-token revocation, only secret rotation. Use short TTLs.
- **The dashboard stores the key in the browser** (local storage). Treat a shared browser as a leaked key and rotate.
- **Root processes inside instances.** Containers start as root (the gateway drops to `agent` for shells and honours the requested exec user), and `agent` has passwordless sudo. Under gVisor this is contained; it is still root inside the sandbox.
- **Side channels and resource exhaustion.** CPU and memory are limited per instance; disk I/O, network bandwidth and PIDs are not, so a noisy instance can degrade its neighbours.
- **VM type.** Experimental and requires the whole daemon to run as root.
- **Audit log.** The ledger records money, not actions; there is no per-request audit trail beyond the daemon's logs.

## Reporting

See [SECURITY.md](../SECURITY.md) for the disclosure address and the 90-day policy.
