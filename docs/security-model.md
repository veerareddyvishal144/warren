# Security model

The one-page model: what Warren trusts, what each runtime isolates, what is not isolated, what is new in this release and what is still missing. [security.md](security.md) is the detailed reference (every edge rule, iptables chain and known limitation); this page is the map. To report a vulnerability, follow [SECURITY.md](../SECURITY.md).

## Assets and trust boundaries

Trusted: the host (its kernel, Docker, Firecracker or Lume, the data directory, the daemon) and the operator. Untrusted: everything that runs inside an instance, and every request from outside until the edge has authenticated it. Workspaces are tenants that do not trust each other.

| Asset | Who holds it | Who must never see it |
|---|---|---|
| Workspace API key (`sk_live_...`) | The client: CLI, MCP server, your backend. Only its SHA-256 is stored | Code inside an instance (the edge strips it from every forwarded request), other workspaces, logs |
| The edge key: the 256-bit HMAC secret at `<data>/secret` (mode 0600) or `--secret` | The daemon alone | Everyone else. It signs every signed URL and session cookie, so whoever holds it can mint access to any instance on the host |
| The per-instance gateway token (`WARREN_INSTANCE_TOKEN`) | The daemon and that instance's gateway | Clients (the edge and the cron scheduler add it), processes inside the instance (it is not exported to exec shells, agent tool shells or the terminal), other instances |
| Instance `env` (credentials you pass at create time) | The database and that instance | Other instances and workspaces. **Everything inside the instance can read it**; the secret broker below exists so that provider keys no longer need to be there |
| Persistent disks, backups, suspended-state files | The data directory, and the Lume VM directories on a Mac | Other instances; anyone without host access |
| Host capacity | The operator | Any single workspace (the awake quota) or any single type (the two-guest host capacity) |

Five boundaries follow from that:

1. **The edge.** Instances have no public addresses and publish no ports. Every request to the API or to an instance passes `warrend`, which authenticates it (key, signed URL or same-origin cookie), checks the instance belongs to the key's workspace, strips the credentials and forwards. Nothing inside an instance ever sees a workspace key.
2. **The instance.** The host does not trust what runs inside. The runtime provides the boundary (next section); the gateway inside requires the instance token on every route except health and version, so even an instance that learns another's address cannot drive it.
3. **The workspace.** Keys, instances, templates and backups of one tenant are invisible to another; probing another tenant's instance id yields `404`, not `401`.
4. **Instance to instance.** One instance cannot reach another over the network: Docker bridges with inter-container communication disabled, isolated bridge ports for Firecracker taps, a `pf` anchor in the macOS base image that blocks the shared NAT subnet except the host gateway and DNS.
5. **Egress** (new in this release, below). What an instance may talk to on the internet, and with which credentials.

## What each runtime isolates

| | `default`: gVisor container | `vm`: Firecracker microVM | `mac`: macOS VM (Virtualization.framework through Lume) |
|---|---|---|---|
| Kernel boundary | gVisor's Sentry serves the application's system calls in user space; a small, audited set reaches the host kernel | The guest's own Linux kernel under KVM | The guest's own macOS kernel under Apple's hypervisor |
| CPU and memory | cgroup limits from the shape; 4096 PIDs | The VM's configuration | The VM's configuration (4 GB minimum) |
| Instance to instance | `enable_icc=false` on both bridges | Isolated bridge ports on `warren-fc0` | `pf` anchor inside each guest, verified from a guest by the e2e script |
| Egress | `allow-all` or `deny-all` by bridge; when the daemon is root, host-wide drops of `169.254.0.0/16` always and of RFC 1918 and the host's own addresses unless `--allow-private-egress`; allowlist new | The same drops in `FORWARD` and `INPUT` for the guest subnet; allowlist new | NAT through the Mac. **`deny-all` is not enforced by the mac runtime in this build** and the daemon installs no host-wide drops on macOS; allowlist new |
| How the daemon runs commands | `docker exec` | The gateway's `/v1/exec` with the instance token, over the daemon's direct route to the guest | Same as `vm` |
| Who can reach the gateway | The daemon only | The daemon only | The daemon only, by the `pf` rules |
| Memory on sleep | Discarded with the container | Discarded with the VM | **Written to disk** next to the VM's disk image, unencrypted, and restored on wake |
| Fallback when the boundary is missing | `runc`: ordinary namespaces on the shared host kernel; development only, and the daemon warns. Set `WARREN_RUNTIME=runsc` in production | None; the type is disabled | None; the type is disabled |

Inside every instance there is deliberately **no privilege boundary**: the agent user has passwordless sudo, exec as root is allowed, and anything the agent runs can change the whole instance including its persistent disk. The runtime boundary is what contains it.

## What is not isolated today

- **Lume's API is unauthenticated and hosts every VM in one process.** `lume serve` listens on `127.0.0.1:7777` with no credentials; any local user or process on the Mac can start, stop, clone or delete every `mac` instance, and if the process dies every running guest dies with it. The daemon supervises and restarts it; nothing authenticates it. Run Warren on a Mac without untrusted local users and nothing else that can reach that port.
- **Suspended macOS memory is on disk in the clear.** Whatever was in a guest's RAM at sleep (including credentials an agent had loaded) sits in the VM directory until the next wake overwrites it. Protect the Mac's disk with FileVault and treat the data directory as sensitive.
- **`network: deny-all` does nothing for `mac` instances** and there are no host-wide egress drops on a Mac. Until the allowlist proxy is verified on your host, a macOS guest can reach whatever the Mac can.
- **Instance env is readable inside the instance**, by design, and stored in plaintext in the database. Use the secret broker for provider keys and keep everything else out of `env`.
- **Subdomains of one registrable domain are same-site to browsers.** Register the base domain on the Public Suffix List or give each tenant its own domain; see [security.md](security.md).
- **TLS is not built in.** The daemon speaks HTTP; terminate TLS in Caddy, Cloudflare or another proxy and never expose 7337 directly.
- **No disk quota** on the Linux bind-mounted home, no I/O or bandwidth limits anywhere, no per-workspace CPU and RAM totals: a noisy instance can degrade its neighbours and fill the data volume.
- **No audit log** beyond the daemon's logs; signed URLs and cookies are bearer credentials with no per-token revocation.
- **The dashboard stores the key in the browser.**

## New in this release: egress allowlist and secret broker

**Status: new.** Both features were implemented alongside this document and have run on the development machines only. The flag and field names are in [API.md](API.md) and [security.md](security.md) as they land; where this page and the contract disagree, the contract wins. Do not rely on either as a boundary on your host until you have run the checks at the end of this section.

### The problem they solve

Before this release an instance had two network policies: `allow-all`, which reaches the whole internet, and `deny-all`, which reaches nothing. An agent needs a third thing: a few named services (the model provider, a package registry, a git host) and nothing else. And to use those services it needed the credentials in its environment, where anything it runs, including code it downloaded, could read them. Both the prompt-injection story and the leaked-key story start there.

### The design

**Egress allowlist.** An instance may carry a per-instance list of allowed destination domains. In that mode the instance has no direct route to the internet; its outbound traffic goes to a proxy on the host, which resolves the destination, checks it against the instance's list (and the host-wide blocks of the metadata and private ranges, which no allowlist can open) and either connects on the instance's behalf or refuses. Because the proxy resolves names and connects itself, the instance cannot bypass the list by using an IP address or by answering its own DNS. `allow-all` and `deny-all` remain as they were.

**Secret broker.** Secrets are stored on the host, scoped to a workspace or to an instance, and never written into the instance's environment or filesystem. The proxy carries **provider routes**: for a configured provider (a model API, a git host, a package registry) the route names the destination host and the header to inject. When an instance sends a request to that provider through the proxy, the proxy adds the real credential (`Authorization: Bearer ...` or the provider's own header) on the way out. The instance addresses the provider as usual and sends no secret; the process inside holds nothing worth stealing; a compromised instance can use the credential only while it runs and only for the destinations its routes allow; rotation happens on the host without touching the instance.

The two compose: a provider route is also an allowlist entry, so an instance with "the model API, the git host and the package registry" allowed and a key brokered for the first two has exactly the reach it needs and no key to leak.

### What must hold, and how to check it

These are the properties that turn the design into a boundary. Check them on your host before trusting it:

1. **No direct egress in allowlist mode.** From inside an instance, a connection that ignores the proxy must fail: `curl --noproxy '*' --max-time 5 https://example.com` should not connect. If it does, the proxy is a convenience, not a boundary.
2. **The list is enforced by the proxy, not by DNS.** A request through the proxy to the IP address of a host that is not on the list must be refused.
3. **The secret never enters the instance.** The environment of exec shells and of the gateway, the process environments visible inside, and the files API must not contain the brokered value; a request to a provider that is *not* on a route must leave with no injected header.
4. **Scope holds.** A secret scoped to instance A must not be injected for instance B, and a workspace's secrets must not be reachable from another workspace's instances.
5. **The blocks still apply.** `169.254.0.0/16` and, without `--allow-private-egress`, the private ranges must be refused by the proxy regardless of the allowlist.

## Known gaps and roadmap

| Gap | Today | Planned |
|---|---|---|
| Secrets at rest | Instance env and brokered secrets are protected by file permissions and the database's location only | Encrypt them at rest with a key held outside the data directory, so a copied data directory or backup does not yield credentials |
| One `lume serve` for every VM | A crash takes every macOS guest down; host-side stats are unavailable per VM | One `lume serve` per instance, so a failure is contained and each VM can be measured from the host |
| Lume's unauthenticated API | Documented; mitigated by running on a single-purpose Mac | Replace Lume with a runtime that talks to Virtualization.framework directly, removing the local API and the shared process |
| Egress on macOS | No `deny-all`, no host-wide drops | `pf` rules on the Mac host for the NAT subnet, the way iptables works on Linux; the allowlist proxy applies to every type |
| Memory on sleep | Linux discards it; macOS writes it to disk unencrypted | Checkpoints for Linux; encrypt saved-state files or wipe them after resume |
| Tenancy on one domain | Documented; the operator's job | Guidance and automation for Public Suffix List registration or per-tenant domains |
| Audit | Daemon logs only | A per-request audit trail with the key id, instance and action |
| Disk quotas and I/O limits | None on the Linux home | Filesystem quotas or sized images for `default`, I/O weights per instance |
| TLS | External proxy | Still an external proxy; the Cloudflare edge item on the roadmap authenticates there too |

## Reporting

Found a way across one of the boundaries above? [SECURITY.md](../SECURITY.md) has the private reporting channel, the response targets and what is in scope. The items in "What is not isolated today" are known; a report that shows one of them is worse than described, or reachable remotely, is very much in scope.
