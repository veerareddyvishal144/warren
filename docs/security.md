# Security

What Warren protects, how, and what it does not protect yet. Read this before putting untrusted code or untrusted users on a host. Vulnerability reports: [SECURITY.md](../SECURITY.md).

## Assets and parties

| Asset | Threatened by |
|---|---|
| The host: its kernel, Docker daemon, other instances, the data directory | Code running inside an instance (agents run arbitrary code by design) |
| A workspace's instances, files, sessions and keys | Other workspaces; anyone on the network who reaches the edge |
| API keys and the signed-URL secret | Leaks through instances, logs, backups, the dashboard |
| The operator's capacity | Abuse of open signup, runaway instances |
| Private networks and cloud metadata reachable from the host | Egress from instances |

Trust boundaries: the edge (everything outside is untrusted), the instance boundary (everything inside is untrusted by the host), the workspace boundary (instances and keys of one tenant are invisible to another), and the instance-to-instance boundary (one instance cannot reach another, even on the same host).

## Edge authentication

Instances never get public IPs and publish no ports; every request passes the edge inside `warrend`, which:

- Classifies the `Host` header (`api`, root, `<id>`, `<id>-<port>`); a label with an extra dot is not an instance host.
- Requires a workspace key (`X-Warren-Key` or `Authorization: Bearer`), a signed URL token or the `warren_session` cookie on every instance request, answering `401 invalid_api_key` otherwise.
- Checks that the instance belongs to the key's workspace and answers `404 not_found` otherwise, so ids of other tenants cannot be probed into existence.
- Strips the credentials before forwarding. `X-Warren-Key`, an `Authorization: Bearer` header that carries a workspace key, and the `warren_session` cookie never reach the code inside an instance.
- Accepts the `warren_session` cookie only for same-origin requests. A page on another site cannot ride on a browser's existing session with an instance.
- Answers CORS on instance hosts for the dashboard origin and the instance's own origins (`<id>.<base>` and `<id>-<port>.<base>`) only. Other origins get no CORS headers.
- Redirects a signed URL to the token-free URL with an absolute `Location` on the instance's own origin after setting the cookie.
- Times out API requests after 60 s and serves at most a fixed number of connections at once (a global concurrency limit on the listener), so one client cannot hold the daemon open indefinitely.

Keys are `sk_live_` plus 40 base62 characters (about 238 bits); only their SHA-256 is stored and the plaintext is shown once. Keys can expire (`expires_at`) and be deleted at any time. Signed URL tokens are HMAC-SHA256 over `<id>|<port>|<expires_at>` with a 256-bit secret kept at `<data>/secret` with mode 0600 (or supplied with `--secret`); tokens are bound to one instance and port and expire after at most 24 hours. Instance ids are 10 random lowercase alphanumerics, but authorization never relies on them being secret.

## Isolation of the default type: gVisor

Default instances are Docker containers run with the `runsc` runtime when it is registered (`--runtime auto` or `runsc`). gVisor interposes a user-space kernel between the container and the host kernel: the application's system calls are served by the Sentry, and only a small, audited set reaches the host. This is the boundary that makes running agent-written code on a shared host acceptable. CPU and memory limits are enforced per container from the instance's shape. `runc` remains as a fallback for development; the daemon logs a warning when it falls back, and you should set `WARREN_RUNTIME=runsc` in production so a missing gVisor is a startup error.

The `vm` type puts a hardware virtualisation boundary (Firecracker, KVM) around the instance instead, at the cost of needing root on the host and being experimental.

## The per-instance token

The only secret injected into an instance is `WARREN_INSTANCE_TOKEN`, a random 40-character token generated per instance and known to the daemon alone. The gateway requires it in `X-Warren-Instance-Token` on every route except `GET /v1/health` and `GET /v1/version`: the Agent API, sessions, the files API, exec and anything else the gateway serves refuse without it. The edge adds the header to every request it forwards to an instance and the cron scheduler adds it to the prompts it fires, so clients authenticate to the edge with a workspace key or signed URL and never see or send the instance token. The edge never forwards a client-supplied `X-Warren-Instance-Token`.

The token is not exported to exec shells, to the shells the agent's tools run, or to the terminal (`ttyd`), so code running in the instance does not find it in its environment. Together with the network isolation below this means the gateway of one instance is reachable only by the daemon: a token that leaks anyway lets its holder talk to that one instance's gateway if they can also reach its IP, which is nothing more than being inside already. The workspace key is never present inside an instance; agent credentials you choose to pass in `env` are, and are visible to anything running there.

## Files confinement

The gateway's files API resolves every path against the home directory and rejects `..`, absolute paths outside it, and symlinks that point outside or dangle, with `403 forbidden`; the home itself cannot be deleted or moved. The daemon's own operations on disks (fork, backup, restore, delete) stay under `<data>/instances/<id>/`. Exec output is capped at 1 MiB per stream and exec commands at 1 MiB, so an instance cannot flood the daemon's memory through those paths; JSON bodies are capped at 4 MiB.

## Secrets at rest

The database holds API key hashes, never plaintext keys. The data directory holds the HMAC secret (0600) and, in instance env records and the `instance_secrets` table, whatever credentials you gave your instances: treat the data directory and its backups as sensitive. Cron prompts are stored in the database. Logs use `tracing`; keys and tokens are not logged by design, but treat log files as sensitive anyway.

## Networking

Instances with `allow-all` sit on the `warren-net` bridge and can reach the internet; `deny-all` moves them to `warren-internal`, a bridge with no route out. Both networks are created with inter-container communication disabled (`com.docker.network.bridge.enable_icc=false`), so an instance cannot reach another instance's IP over the bridge: it sees the bridge gateway and, on `warren-net`, the outside world, and nothing else on the host. Firecracker tap devices are attached to their bridge with port isolation for the same effect.

**Upgrading from an earlier build.** Earlier builds created the networks with inter-container communication enabled, and Docker does not change the options of an existing network. Stop the instances, run `docker network rm warren-net warren-internal`, and restart `warrend`; it recreates both networks with the new options and instances attach to them on their next start. Until you do this, instances on the same bridge can still reach each other.

**Egress policy.** When `warrend` runs as root it installs iptables rules at startup in the `DOCKER-USER` chain, which Docker evaluates before its own forwarding rules, and `FORWARD` rules for the Firecracker bridge. They drop traffic from instances to:

- `169.254.0.0/16`, always. This is the cloud metadata range (instance credentials on AWS, GCP, Azure and others); no flag opens it.
- The RFC 1918 ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`) and the host's own addresses, unless `--allow-private-egress` (`WARREN_ALLOW_PRIVATE_EGRESS=true`) is set. Set it only when instances must reach services on your private network; it does not open the metadata range.

Check the rules after the first start with `sudo iptables -S DOCKER-USER`. When the daemon is not root it cannot install them: it logs a warning and installs nothing, and instances can then reach whatever the host can reach. Run the daemon as root in production, or install equivalent rules yourself before starting instances.

**Egress allowlist and the secret broker.** `network: allowlist` puts the instance on the no-route-out bridge like `deny-all` and hands it proxy variables pointing at the daemon's egress broker (`--broker-listen`, default `0.0.0.0:7349`; `--broker-advertise` names the address instances use when the derived one is wrong). The broker identifies the instance by the source address of its connections (registered when the runtime reports the address, removed on stop, sleep, delete or failure), forwards `http://` requests and `CONNECT` tunnels only to hosts on that instance's `allowed_domains`, resolves names itself and never connects to loopback, private, link-local or other reserved addresses, whatever the list says. Because the proxy resolves and connects, an instance cannot bypass the list with an IP literal or its own DNS answers; because it is HTTP-only, nothing else leaves an `allowlist` instance. The same broker serves **secret routes**: a secret `{ name, upstream, header, value, env }` becomes a private URL `http://<broker>/r/<token>` in the instance's `<env>` variable; requests to it are rewritten onto the `https://` upstream with the credential header added. The value is stored on the host only (plaintext in the database, like `env`), never written into the instance's environment or filesystem, never returned by the API, and the token is honoured only from the instance's own address, so a leaked route URL is useless from anywhere else and dies with the instance. On a Linux host the broker port is the one thing an `allowlist` or `deny-all` instance may reach on the host (an `INPUT` accept for the bridge gateway, ahead of the host-address drops); on a Mac the guest's `pf` rules allow the host gateway only (see [security-model.md](security-model.md)). The broker listens on every interface by default so that Lume guests (`192.168.64.1`) and Docker bridges can reach it; it refuses connections from addresses it has no registered instance for, but on a host with public interfaces bind it to the bridge addresses or firewall the port.

## Signup and abuse

`POST /v1/signup` is served while `--signup-enabled` is true (`WARREN_SIGNUP_ENABLED`, default `true`) and creates a workspace with its first key for anyone who can reach the API host. Signups are limited to 10 per source IP per hour; the loopback address is exempt from the limit in `--dev` so local scripts and tests are not throttled. For a hosted deployment either turn open signup off with `--signup-enabled=false` and create workspaces from your own flow, or keep the API host reachable only from your own backend and call `POST /v1/signup` from there after your own checks. In either case set `--max-awake-instances` so a single workspace cannot occupy the whole host ([quotas.md](quotas.md)). Only signup is rate limited; the rest of the API relies on the key, the 60 s request timeout and the listener's concurrency limit, so put a proxy with rate limits in front on a public host.

## Known limitations

Be explicit about these when you decide what to run.

- **Instance subdomains share a registrable domain.** Instances are `<id>.<base-domain>`, and browsers treat subdomains of one registrable domain as same-site: cookies set for the base domain, and some same-site checks, would span tenants. Register the base domain on the [Public Suffix List](https://publicsuffix.org/) or give each tenant its own domain, so that instance origins are cross-site to each other. Until then, browser-side isolation between instances of different tenants is weaker than between unrelated sites.
- **The allowlist is HTTP(S) through a proxy.** An `allowlist` instance reaches its domains over HTTP and `CONNECT` tunnels only; DNS, SSH, raw TCP and UDP have no path out. The list matches host names, not paths or methods: an allowed host is fully usable. Hosts that front many unrelated services (object stores, CDNs, pastebins) are an exfiltration path if you allow them. The pf-based enforcement inside `mac` guests needs a base image built from this release; older images apply no per-instance policy.
- **Secrets are protected by the boundary, not by cryptography.** A compromised instance can use its routes for as long as it runs, for the upstreams its routes name; the broker adds the credential to whatever request arrives at the route. Rotate on the host with a `PATCH` and keep `allowed_domains` to the hosts the agent needs.
- **Root inside the instance, by design.** Containers start as root, the gateway drops to `agent` for shells and honours the requested exec user, and `agent` has passwordless sudo inside its own instance; `exec` as `root` is allowed. Agents need this to install packages and change system files. Under gVisor or in a VM it is contained, but there is no privilege boundary inside the instance: assume anything the agent runs can change the whole instance, including its persistent disk.
- **`runc` fallback.** Without gVisor the container shares the host kernel with ordinary namespacing only. Fine for your own code, not for untrusted code.
- **Disk quotas.** The disk size is a sizing number; the bind-mounted home has no filesystem quota in this build, so an instance can fill the data volume. Watch `disk_used_bytes` and free space on the host.
- **Only the awake count is capped.** `--max-awake-instances` limits how many instances a workspace keeps awake; their combined vCPUs, RAM and disk are not, so size the quota for the largest shape ([quotas.md](quotas.md)).
- **TLS.** The daemon speaks plain HTTP; TLS comes from Caddy, Cloudflare or another proxy ([self-hosting.md](self-hosting.md)). Do not expose 7337 directly.
- **Signed URLs and cookies are bearer credentials.** Anyone who gets the URL can use it until it expires; the cookie keeps a browser authenticated on that host for the token's lifetime (same-origin requests only). There is no per-token revocation, only secret rotation. Use short TTLs.
- **The dashboard stores the key in the browser** (local storage). Treat a shared browser as a leaked key and rotate.
- **Side channels and resource exhaustion.** CPU and memory are limited per instance; disk I/O, network bandwidth and PIDs are not, so a noisy instance can degrade its neighbours.
- **VM type.** Experimental and requires the whole daemon to run as root.
- **Audit log.** There is no per-request audit trail beyond the daemon's logs.

## Reporting

See [SECURITY.md](../SECURITY.md) for the disclosure address and the 90-day policy.
