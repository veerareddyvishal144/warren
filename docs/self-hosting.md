# Self-hosting

Warren runs as one process on one Linux host. This page sets it up for real use: a data directory, a systemd service, a wildcard domain with TLS in front, and backups. It assumes Debian 12 or Ubuntu 22.04 or newer on x86_64.

## 1. Host

Size the machine for the instances you want awake at once: their vCPUs and RAM add up (the daemon enforces CPU and memory limits per container), plus about 1 GB for Docker and the daemon; set `--max-awake-instances` ([quotas.md](quotas.md)) so one workspace cannot take all of it. Put the data directory on a volume with room for every instance's disk, the backups and the images (about 700 MB for `warren/base`). The host needs outbound internet for image builds and for instances with `allow-all`.

## 2. Docker

```bash
sudo apt-get update && sudo apt-get install -y ca-certificates curl git build-essential pkg-config musl-tools
curl -fsSL https://get.docker.com | sudo sh
sudo systemctl enable --now docker
docker info >/dev/null && echo docker ok
```

If the daemon will run as a non-root user, add that user to the `docker` group. Running the daemon as root is simpler and is required for two things: the egress firewall rules in section 8 (a non-root daemon logs a warning and installs none) and `vm` instances. The rest of this page assumes root.

## 3. gVisor

```bash
sudo scripts/install-gvisor.sh
docker run --rm --runtime=runsc alpine uname -r        # prints a gVisor kernel version
```

The script downloads the release tarball, verifies its SHA-512, installs `runsc` and `containerd-shim-runsc-v1` into `/usr/local/bin`, merges the `runsc` runtime (with `--platform=systrap`) into `/etc/docker/daemon.json` without touching other keys, reloads Docker and runs the smoke test. Re-run it to upgrade. Set `WARREN_RUNTIME=runsc` in production so a missing gVisor is a startup error rather than a silent fallback to `runc`.

## 4. Build

```bash
curl https://sh.rustup.rs -sSf | sh -s -- -y && source ~/.cargo/env
rustup target add x86_64-unknown-linux-musl
git clone https://github.com/veerareddyvishal144/warren.git && cd warren
cargo build --release --locked -p warrend -p warren-cli
make gateway                                           # static musl warren-gateway for the images and for injection
sudo install -m 0755 target/release/warrend /usr/local/bin/warrend
sudo install -m 0755 target/release/warren /usr/local/bin/warren
sudo install -m 0755 target/x86_64-unknown-linux-musl/release/warren-gateway /usr/local/bin/warren-gateway
WARREN_IMAGE_TAG=prod WARREN_SKIP_GATEWAY_BUILD=1 scripts/build-images.sh   # warren/base:prod, warren/agent:prod, warren/opencode:prod
```

Images live in the local Docker daemon; start `warrend` with the same `--image-tag`.

## 5. Data directory and configuration

```bash
sudo mkdir -p /var/lib/warren /etc/warren
sudo chmod 0700 /var/lib/warren
```

```
/var/lib/warren/
  warren.db                 SQLite (plus warren.db-wal and warren.db-shm while running)
  secret                    HMAC secret for signed URLs, 0600, generated on first start
  instances/<id>/home       each instance's persistent disk
  instances/<id>/vm/        rootfs and logs of vm-type instances
  backups/<id>/<backupId>.tar.gz
```

Configuration is flags or `WARREN_*` variables; write them to `/etc/warren/warrend.env`:

```bash
# /etc/warren/warrend.env
WARREN_LISTEN=127.0.0.1:7337            # only the TLS proxy talks to the daemon
WARREN_BASE_DOMAIN=warren.example.com   # api.warren.example.com, <id>.warren.example.com, <id>-<port>.warren.example.com
WARREN_PUBLIC_SCHEME=https
WARREN_PUBLIC_PORT=443                  # omitted from URLs because it is the scheme default
WARREN_DATA_DIR=/var/lib/warren
WARREN_RUNTIME=runsc                    # auto | runsc | runc
WARREN_IMAGE_TAG=prod
WARREN_REGION=fra1                      # label reported on instances
WARREN_LOG=info                         # tracing filter, e.g. info,warren_edge=debug
# WARREN_DEV=false                      # never true in production: 5 s idle timeouts, host.docker.internal
# WARREN_SECRET=                        # generated into <data>/secret when unset
# WARREN_MAX_AWAKE_INSTANCES=           # awake instances per workspace; unset = unlimited
# WARREN_MIN_IDLE_SECONDS=300           # lowest idle timeout instances may ask for
# WARREN_ENABLE_FIRECRACKER=false       # see vm-instances.md
# WARREN_FIRECRACKER_BIN=/usr/local/bin/firecracker
# WARREN_KERNEL_IMAGE=
```

| Variable | Flag | Default | Notes |
|---|---|---|---|
| `WARREN_LISTEN` | `--listen` | `:7337` | `:port` binds dual-stack on every interface; give an address to restrict it |
| `WARREN_BASE_DOMAIN` | `--base-domain` | `warren.localhost` | Must have wildcard DNS and a wildcard certificate |
| `WARREN_PUBLIC_SCHEME` | `--public-scheme` | `http` | Scheme in URLs the API returns and signs |
| `WARREN_PUBLIC_PORT` | `--public-port` | `7337` | Port in URLs; 443 or 80 are omitted; 0 omits always |
| `WARREN_DATA_DIR` | `--data-dir` | `/var/lib/warren` | Everything persistent |
| `WARREN_RUNTIME` | `--runtime` | `auto` | Docker OCI runtime for `default` instances |
| `WARREN_REGION` | `--region` | `local` | Reported on instances; the multi-node scheduler will use it |
| `WARREN_DEV` | `--dev` | `false` | 5 s minimum idle timeout and `host.docker.internal` inside instances; never in production |
| `WARREN_MAX_AWAKE_INSTANCES` | `--max-awake-instances` | unset | Cap on awake instances per workspace; unset means unlimited ([quotas.md](quotas.md)) |
| `WARREN_MIN_IDLE_SECONDS` | `--min-idle-seconds` | `300` (5 in dev) | |
| `WARREN_SECRET` | `--secret` | generated | Rotating it invalidates every signed URL and cookie |
| `WARREN_IMAGE_TAG` | `--image-tag` | `dev` | Tag of the built-in images |
| `WARREN_ENABLE_FIRECRACKER`, `WARREN_FIRECRACKER_BIN`, `WARREN_KERNEL_IMAGE` | `--firecracker`, `--firecracker-bin`, `--kernel` | off | [vm-instances.md](vm-instances.md) |
| `WARREN_GATEWAY_BIN` | `--gateway-bin` | next to `warrend`, else `/usr/local/bin/warren-gateway` | Static gateway injected into registered images |
| `WARREN_SIGNUP_ENABLED` | `--signup-enabled` | `true` | `false` makes `POST /v1/signup` answer 404 |
| `WARREN_ALLOW_PRIVATE_EGRESS` | `--allow-private-egress` | `false` | Lets instances reach RFC 1918 ranges and the host; 169.254.0.0/16 stays blocked |
| `WARREN_LOG` | `--log` | `info` | tracing `EnvFilter` syntax |

Signup is on by default (`POST /v1/signup` needs no auth) and limited to 10 signups per client IP per hour. On a public host turn it off with `--signup-enabled false` and create workspaces through your own flow, and set `WARREN_MAX_AWAKE_INSTANCES` so an anonymous workspace cannot take the whole host.

## 6. systemd

```ini
# /etc/systemd/system/warrend.service
[Unit]
Description=Warren daemon (sandbox API, edge, lifecycle, cron)
After=network-online.target docker.service
Wants=network-online.target
Requires=docker.service

[Service]
Type=simple
EnvironmentFile=/etc/warren/warrend.env
ExecStart=/usr/local/bin/warrend
Restart=always
RestartSec=3
LimitNOFILE=1048576
KillSignal=SIGTERM
TimeoutStopSec=30

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now warrend
sudo journalctl -u warrend -f
curl -s http://127.0.0.1:7337/v1/health -H 'Host: api.warren.example.com'
```

On `SIGTERM` the daemon stops its loops and the listener; containers keep running under Docker, and the reconciler adopts them at the next start, fixing any transitional states. Restarting the daemon therefore does not restart instances.

## 7. DNS and TLS

You need two names pointing at the host: the base domain and a wildcard under it.

```
warren.example.com.     A     203.0.113.10
*.warren.example.com.   A     203.0.113.10
```

The certificate must cover `warren.example.com` and `*.warren.example.com`. Wildcards are only issued through the ACME DNS-01 challenge, so the proxy (or certbot) needs API access to your DNS provider. The proxy must preserve the `Host` header (the daemon dispatches on it) and pass WebSockets (ttyd needs them); both options below do.

### Option A: Caddy

Use a Caddy build with your DNS provider's module (download one from caddyserver.com with the module selected, or `xcaddy build --with github.com/caddy-dns/cloudflare`):

```
# /etc/caddy/Caddyfile
warren.example.com, *.warren.example.com {
    tls {
        dns cloudflare {env.CLOUDFLARE_API_TOKEN}
    }
    reverse_proxy 127.0.0.1:7337
}
```

Caddy obtains and renews the wildcard certificate, terminates TLS on 443, forwards the `Host` header and sets `X-Forwarded-For` and `X-Forwarded-Proto`. Put the token in Caddy's environment (`/etc/caddy/caddy.env` referenced from the unit, or a systemd override). If you already have a wildcard certificate, replace the `tls` block with `tls /etc/ssl/warren/fullchain.pem /etc/ssl/warren/privkey.pem`.

### Option B: Cloudflare in front

With the zone on Cloudflare you can keep TLS at their edge:

- **Cloudflare Tunnel**: install `cloudflared`, create a tunnel, and route both hostnames to the daemon:

  ```yaml
  # /etc/cloudflared/config.yml
  tunnel: <tunnel-id>
  credentials-file: /etc/cloudflared/<tunnel-id>.json
  ingress:
    - hostname: warren.example.com
      service: http://127.0.0.1:7337
    - hostname: "*.warren.example.com"
      service: http://127.0.0.1:7337
    - service: http_status:404
  ```

  then `cloudflared tunnel route dns <tunnel> warren.example.com` and a CNAME for `*.warren.example.com` to `<tunnel-id>.cfargotunnel.com`. No inbound port is open on the host.
- **Proxied DNS** (orange cloud) to the host, with Caddy or an origin certificate terminating TLS on 443 as in option A.

Mind the certificate depth: Cloudflare's Universal SSL covers `example.com` and `*.example.com`, not `*.warren.example.com`. Either use the zone apex as the base domain (`WARREN_BASE_DOMAIN=example.com`, so instances are `ab12cd34ef.example.com`) or add an Advanced Certificate for `*.warren.example.com`. WebSockets are proxied by default. Cloudflare's 100-second proxy read timeout is enough for wake-on-request; long-running Agent API turns should stream.

### Check

```bash
curl -s https://api.warren.example.com/v1/health
curl -s -X POST https://api.warren.example.com/v1/signup -H 'Content-Type: application/json' -d '{"name":"ops"}'
```

The instance `url` fields now read `https://<id>.warren.example.com`, the dashboard is at `https://warren.example.com/`, and signed URLs open in any browser.

## 8. Firewall

Expose 443 (and 80 if you use HTTP-01 for the apex). Keep 7337 on loopback. Instances have no published ports: all traffic enters through the edge.

The daemon creates two Docker bridge networks, `warren-net` (with egress) and `warren-internal` (no route out), both with inter-container communication disabled, so instances cannot reach each other over the bridge; Firecracker taps get bridge port isolation. When it runs as root it also installs iptables rules at startup in the `DOCKER-USER` chain (and `FORWARD` rules for the Firecracker bridge) that drop traffic from instances to `169.254.0.0/16` (cloud metadata) always, and to the RFC 1918 ranges and the host's own addresses unless `--allow-private-egress` (`WARREN_ALLOW_PRIVATE_EGRESS=true`) is set. Check them after the first start with `sudo iptables -S DOCKER-USER`. When the daemon is not root it logs a warning and installs nothing; block the private ranges with your own host firewall in that case. Set `--allow-private-egress` only when instances must reach services on your private network (a database, an internal registry); it does not open the metadata range. There is no per-instance allowlist yet: an instance with `allow-all` can reach the whole internet (see [security.md](security.md)).

**Upgrading from a build that created the networks without isolation**: Docker keeps the options of an existing network, so remove the old ones once. Stop the instances, run `docker network rm warren-net warren-internal`, and restart `warrend`; it recreates both networks and instances attach to them on their next start.

## 9. Backups of the data directory

Everything is under `WARREN_DATA_DIR`. Back it up with the daemon running:

```bash
BK=/backup/warren/$(date +%F)
mkdir -p "$BK"
sqlite3 /var/lib/warren/warren.db ".backup '$BK/warren.db'"        # consistent copy despite WAL
cp -p /var/lib/warren/secret "$BK/secret"
tar czf "$BK/instances.tar.gz" -C /var/lib/warren instances         # live homes; stop or sleep instances first for a quiet copy
tar czf "$BK/backups.tar.gz"   -C /var/lib/warren backups
```

- `warren.db` holds workspaces, key hashes, templates, instances, crons, metrics and backup records. Never copy it with plain `cp` while the daemon runs; use `.backup` or stop the daemon.
- `secret` signs URLs and cookies. Losing it only invalidates outstanding signed URLs; leaking it lets anyone mint them, so keep the backup as private as the host.
- `instances/<id>/home` is the users' data. For a crash-consistent copy of a busy instance, take a Warren backup (`POST /v1/instances/{id}/backups`, which archives the home without stopping it) and include `backups/` in the host backup.
- Instance disks and the VM rootfs images are plain files; filesystem snapshots (ZFS, btrfs, LVM) of the data directory are the simplest whole-host backup.

Restore: stop the daemon, put the files back with the same paths and ownership, start the daemon. The reconciler adopts containers that still exist and recreates the rest on their next start.

## 10. Upgrades

```bash
cd warren && git pull
cargo build --release --locked -p warrend -p warren-cli
make gateway
sudo install -m 0755 target/release/warrend /usr/local/bin/warrend
sudo install -m 0755 target/release/warren /usr/local/bin/warren
sudo install -m 0755 target/x86_64-unknown-linux-musl/release/warren-gateway /usr/local/bin/warren-gateway
sudo systemctl restart warrend                       # instances keep running; the reconciler re-adopts them
WARREN_IMAGE_TAG=prod WARREN_SKIP_GATEWAY_BUILD=1 scripts/build-images.sh   # new gateway inside the images
```

Instances pick up a rebuilt image when they are recreated: `POST /v1/instances/{id}/update` (same template, latest revision) does it on demand, otherwise the next stop and start or sleep and wake does. Database migrations are embedded and run at startup; keep the pre-upgrade `.backup` until you are happy.
