# Installing Warren

Warren is four binaries, built and published together for every release:

| Binary | What it is | Runs where |
|---|---|---|
| `warrend` | The daemon: Sandbox API, edge proxy, lifecycle, cron | The host (Linux with Docker, or an Apple silicon Mac for `mac` instances) |
| `warren` | The CLI | Anywhere |
| `warren-mcp` | The MCP server for Claude Code, Cursor, Claude Desktop and Windsurf | Next to the editor |
| `warren-gateway` | The agent-side gateway the daemon places inside every instance | Inside instances; the daemon only needs the file |

Prebuilt binaries exist for three platforms. Every release ships one tarball per platform plus a `SHA256SUMS` file:

| Tarball | For |
|---|---|
| `warren-<version>-darwin-arm64.tar.gz` | macOS on Apple silicon (macOS 14 or newer). No Intel build: the `mac` instance type needs Apple silicon, so Intel Macs [build from source](#from-source). |
| `warren-<version>-linux-x86_64.tar.gz` | Linux x86_64 with glibc 2.35 or newer: Ubuntu 22.04, Debian 12, Fedora 36 and newer. `warren-gateway` in it is a fully static musl binary. |
| `warren-<version>-linux-arm64.tar.gz` | Linux arm64 (aarch64), same glibc floor. |

Alpine and other musl-based distributions are not covered by the tarballs (only the gateway is static); use the [Docker image](#docker-image) or build from source there.

Installing puts the binaries in place; running the daemon is the next step. After any of the methods below:

```sh
warren up                # start the daemon on this machine
warren mac init          # macOS only: prepare this Mac for macOS instances (Lume, base VM)
warren connect claude    # add the Warren MCP server to Claude Code
```

For a Linux server with Docker, gVisor, a domain and TLS, continue with [self-hosting.md](self-hosting.md); the template images (`warren/base`, `warren/agent`, `warren/opencode`) are built on the host with `scripts/build-images.sh` from a checkout of the repository, pointing `WARREN_GATEWAY_BIN` at the installed `warren-gateway`.

## Homebrew (macOS and Linux)

The tap is `veerareddyvishal144/homebrew-warren`:

```sh
brew tap veerareddyvishal144/warren
brew install warren
```

(or `brew install veerareddyvishal144/warren/warren` in one go). The formula installs the release tarball for your platform, so no Rust toolchain is downloaded. `brew services start warren` runs the daemon with its defaults (data under `$(brew --prefix)/var/warren`, the API at `http://api.warren.localhost:7337`); for `mac` instances the daemon needs `--mac-base-vm`, which the service block cannot take, so use `warren up` (it writes the [launchd agent](#macos-launchd) with the right flags) and `warren mac init` instead.

Upgrade with `brew upgrade warren`; remove with `brew uninstall warren` (and `brew untap veerareddyvishal144/warren`).

## Install script

```sh
curl -fsSL https://raw.githubusercontent.com/veerareddyvishal144/warren/main/scripts/install.sh | sh
```

The script detects the OS and CPU, resolves the latest release (or the one in `WARREN_VERSION`), downloads the tarball and the release's `SHA256SUMS`, verifies the digest, and installs the four binaries into `/usr/local/bin` when that is writable, else into `~/.local/bin`. It never runs `sudo`; for a system-wide install on a machine where `/usr/local/bin` is root-owned, run it as root:

```sh
curl -fsSL https://raw.githubusercontent.com/veerareddyvishal144/warren/main/scripts/install.sh | sudo sh
```

Settings, as environment variables or flags after `sh -s --`:

| Variable | Flag | Meaning |
|---|---|---|
| `WARREN_VERSION` | `--version X.Y.Z` | Release to install (with or without the `v`); default is the latest release |
| `WARREN_INSTALL_DIR` | `--install-dir DIR` | Target directory; created if missing, must be writable |
| `WARREN_REPO` | | GitHub repository, for forks (default `veerareddyvishal144/warren`) |
| `WARREN_BASE_URL` | | Where the tarball and `SHA256SUMS` are fetched from, for mirrors and air-gapped hosts |

```sh
curl -fsSL .../install.sh | WARREN_VERSION=0.1.0 sh
curl -fsSL .../install.sh | sh -s -- --install-dir ~/bin
```

The script is POSIX `sh` (dash, bash, zsh and BusyBox ash all run it) and refuses to install when `SHA256SUMS` is missing or does not match. On Linux it checks the glibc version first and explains what to do on an older or musl-based system.

## Manual install from a tarball

1. Download the tarball for your platform and `SHA256SUMS` from the [releases page](https://github.com/veerareddyvishal144/warren/releases):

   ```sh
   V=0.1.0
   P=linux-x86_64        # or darwin-arm64, linux-arm64
   curl -fsSLO https://github.com/veerareddyvishal144/warren/releases/download/v$V/warren-$V-$P.tar.gz
   curl -fsSLO https://github.com/veerareddyvishal144/warren/releases/download/v$V/SHA256SUMS
   ```

2. Verify:

   ```sh
   sha256sum --check --ignore-missing SHA256SUMS          # Linux
   shasum -a 256 --check --ignore-missing SHA256SUMS      # macOS
   ```

   The output must say `warren-$V-$P.tar.gz: OK`. `SHA256SUMS` lists every tarball of the release; `--ignore-missing` skips the ones you did not download.

3. Unpack and install. The tarball holds one directory with the four binaries and the LICENSE:

   ```sh
   tar -xzf warren-$V-$P.tar.gz
   sudo install -m 0755 warren-$V-$P/warrend warren-$V-$P/warren warren-$V-$P/warren-mcp warren-$V-$P/warren-gateway /usr/local/bin/
   ```

   Keep `warren-gateway` next to `warrend` or in `/usr/local/bin`: the daemon looks in those two places (or takes `--gateway-bin`).

macOS notes: the binaries are ad-hoc signed, which is what Apple silicon requires to run them at all, but they are not notarized. A tarball downloaded with `curl` runs as is; one saved by a browser carries the quarantine attribute, and Gatekeeper then blocks the binaries until you clear it: `xattr -d com.apple.quarantine /usr/local/bin/warrend` (and the other three), or on the tarball before unpacking.

## Docker image

The repository root has a `Dockerfile` that builds `warrend`, `warren`, `warren-mcp` and the static `warren-gateway` into a `debian:bookworm-slim` image, for running the Linux daemon in a container that drives the host's Docker. No image is published yet; build it from a checkout:

```sh
git clone https://github.com/veerareddyvishal144/warren.git && cd warren
docker build -t warren:local .
docker compose -f packaging/docker-compose.yml up -d
curl -s -H 'Host: api.warren.localhost' http://127.0.0.1:7337/v1/health
```

[`packaging/docker-compose.yml`](../packaging/docker-compose.yml) carries the settings the daemon cannot work without, and they are not optional:

- `network_mode: host`. The edge binds `:7337` on the host and reaches instances at their addresses on the `warren-net` bridge. A container on any other network cannot (the daemon creates `warren-net` with inter-container communication off), so there is no port mapping; 7337 is open on the host.
- `cap_add: [NET_ADMIN, NET_RAW]` and `iptables` in the image. As root the daemon installs the instance egress rules in the `DOCKER-USER` chain and refuses to start when it cannot. On a host whose Docker uses the legacy tables (`iptables -S DOCKER-USER` on the host shows them only with `iptables-legacy`), set `WARREN_IPTABLES=legacy`.
- `/var/run/docker.sock` mounted. The daemon is a Docker client; instances are containers of the host's dockerd, not of the daemon's container.
- `/var/lib/warren:/var/lib/warren`, the data directory at the **same path** inside and outside. The daemon asks dockerd to bind-mount instance homes (`<data>/instances/<id>/home`) and the gateway (`<data>/bin/warren-gateway`, which the entrypoint copies there) by host path, and dockerd resolves those paths on the host. Change both sides of the volume and `WARREN_DATA_DIR` together.

What the container does not bring:

- **gVisor**. `runsc` must be installed and registered in the host's dockerd (`sudo scripts/install-gvisor.sh` on the host). Without it `WARREN_RUNTIME=auto` falls back to `runc` with a warning; set `WARREN_RUNTIME=runsc` in production so the fallback is an error.
- **The template images**. `warren/base`, `warren/agent` and `warren/opencode` live in the host's Docker and are built there: `WARREN_GATEWAY_BIN=<static warren-gateway> WARREN_IMAGE_TAG=dev scripts/build-images.sh` from a checkout, with the same `WARREN_IMAGE_TAG` the container uses. The static gateway is the one from a Linux release tarball or `docker cp warrend:/usr/local/bin/warren-gateway .`.
- **The `vm` (Firecracker) and `mac` (Lume) instance types.** They need `/dev/kvm`, tap devices and root on the host, or an Apple silicon Mac; run the daemon natively for those.

Configuration is `WARREN_*` variables in the `environment:` block ([self-hosting.md](self-hosting.md) lists them). `docker run --rm warren:local warren --help` runs the CLI from the image; any command other than `warrend` bypasses the entrypoint's checks.

## Running the daemon as a service

### Linux: systemd

[`packaging/systemd/warrend.service`](../packaging/systemd/warrend.service) is a system unit that runs `/usr/local/bin/warrend` as root with its configuration in `/etc/warren/warrend.env`, restarts it always, and applies the hardening directives that still let it reach the Docker socket. [`packaging/systemd/warrend.env.example`](../packaging/systemd/warrend.env.example) is the configuration file to start from.

```sh
sudo install -m 0644 packaging/systemd/warrend.service /etc/systemd/system/warrend.service
sudo install -d -m 0750 /etc/warren
sudo install -m 0640 packaging/systemd/warrend.env.example /etc/warren/warrend.env
sudoedit /etc/warren/warrend.env            # base domain, runtime, image tag, signup
sudo systemctl daemon-reload
sudo systemctl enable --now warrend
sudo journalctl -u warrend -f
```

The unit expects the binaries in `/usr/local/bin` (a release tarball or the install script run as root); edit `ExecStart` for another location, for example Homebrew on Linux. The data directory is `/var/lib/warren` (`StateDirectory=`); if you move it somewhere outside `/var`, add a `ReadWritePaths=` line. The comments in the unit list the hardening directives that are deliberately off (kernel tunables, SUID restriction, private devices) and why: they break the `vm` type. Enable them on a host that only runs `default` instances.

### macOS: launchd

[`packaging/launchd/dev.warren.daemon.plist`](../packaging/launchd/dev.warren.daemon.plist) is a LaunchAgent template: it runs `warrend --runtime none --mac-base-vm <VM> --data-dir ~/.warren/data` as the logged-in user, keeps it alive, and logs to `~/Library/Logs/Warren/warrend.log`. launchd does not expand `~`, so the template has three placeholders to fill in: `__WARREND__` (the binary's absolute path), `__HOME__` and `__BASE_VM__` (the base VM from [macos.md](macos.md), for example `warren-macos-26`).

```sh
mkdir -p ~/Library/LaunchAgents ~/Library/Logs/Warren ~/.warren
sed -e "s|__WARREND__|$(command -v warrend)|g" -e "s|__HOME__|$HOME|g" -e "s|__BASE_VM__|warren-macos-26|g" \
    packaging/launchd/dev.warren.daemon.plist > ~/Library/LaunchAgents/dev.warren.daemon.plist
plutil -lint ~/Library/LaunchAgents/dev.warren.daemon.plist
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/dev.warren.daemon.plist
tail -f ~/Library/Logs/Warren/warrend.log
```

`--runtime none` skips Docker; drop that pair on a Mac that also runs Docker Desktop for `default` instances. Stop and remove with `launchctl bootout gui/$(id -u)/dev.warren.daemon` and delete the plist. `warren up` renders and loads this plist for you (data under `~/Library/Application Support/Warren`, Docker used when `docker info` works), `warren mac init` builds the base VM, and `warren down --purge` removes both the agent and the data again ([quickstart.md](quickstart.md)).

## Verifying an installation

```sh
warren --version            # warren 0.1.0
warrend --version           # warrend 0.1.0
warren-gateway --version    # warren-gateway 0.1.0
warren health               # once the daemon runs: {"healthy":true,"version":"0.1.0",...}
```

`warren-mcp` reads its settings before parsing flags and needs `WARREN_API_KEY` set, so it has no quick version check; `warren connect claude` configures it.

## Upgrading

Install the new version with the method you used (`brew upgrade warren`, run the install script again, or replace the four files from the new tarball; for the Docker image rebuild and `docker compose up -d`), then restart the daemon:

```sh
sudo systemctl restart warrend                                 # systemd
launchctl kickstart -k gui/$(id -u)/dev.warren.daemon          # launchd
brew services restart warren                                   # Homebrew services
```

Restarting the daemon does not restart instances: on `SIGTERM` it stops its loops and listener, the containers (or VMs) keep running, and the reconciler adopts them at the next start. Database migrations are embedded and run at startup; take a backup of the data directory first ([self-hosting.md](self-hosting.md), section 9).

Instances get the new gateway when their image carries it. After upgrading on Linux, rebuild the template images with the new `warren-gateway` (`WARREN_GATEWAY_BIN=$(command -v warren-gateway) WARREN_IMAGE_TAG=<tag> scripts/build-images.sh`); workspace images that do not ship the gateway get the installed one injected at their next start. Running instances pick up a rebuilt image on their next stop and start, sleep and wake, or `POST /v1/instances/{id}/update`.

## Uninstalling

1. Stop the daemon and remove the service: `sudo systemctl disable --now warrend && sudo rm /etc/systemd/system/warrend.service && sudo systemctl daemon-reload`, or `launchctl bootout gui/$(id -u)/dev.warren.daemon && rm ~/Library/LaunchAgents/dev.warren.daemon.plist`, or `brew services stop warren`, or `docker compose -f packaging/docker-compose.yml down`.
2. Remove the instances you no longer want while the daemon is still available (`warren instances delete <id>`, or `DELETE /v1/instances/{id}`); the daemon removes their containers, VMs and disks. Leftovers after the daemon is gone: containers named `warren-<id>` (`docker ps -a --filter name=warren-`), the networks `warren-net` and `warren-internal`, the images `warren/base`, `warren/agent` and `warren/opencode`, the `DOCKER-USER` iptables rules the daemon added (gone at the next reboot; `sudo iptables -S DOCKER-USER` lists them), and on a Mac the Lume VMs `warren-<id>` (`lume ls`).
3. Remove the binaries: `brew uninstall warren`, or `rm /usr/local/bin/{warrend,warren,warren-mcp,warren-gateway}` (or wherever the install script put them, `~/.local/bin` without root).
4. Remove the data if you do not want it: `/var/lib/warren` (Linux, systemd, Docker), `~/.warren` (the launchd template), `$(brew --prefix)/var/warren` (Homebrew services), plus `/etc/warren` and `~/Library/Logs/Warren`. `warren.db` holds the workspaces and keys and `secret` signs URLs; both are gone with the directory.

## From source

Any platform with a Rust toolchain (1.90 or newer) builds all four binaries; the only extra for Linux is the musl target of the gateway:

```sh
git clone https://github.com/veerareddyvishal144/warren.git && cd warren
cargo build --release --locked -p warrend -p warren-cli -p warren-mcp
make musl-setup && make gateway        # Linux: static warren-gateway; on macOS: cargo build --release --locked -p warren-gateway
```

[self-hosting.md](self-hosting.md) section 4 has the install commands, [DEVELOPMENT.md](DEVELOPMENT.md) the toolchain details. `scripts/release/package.sh` turns such a build into the same tarball the releases ship (see below), which is the way to produce binaries for a platform without a prebuilt one.

## For maintainers: cutting a release

Releases are built by [`.github/workflows/release.yml`](../.github/workflows/release.yml) when a tag `v<version>` is pushed. The version comes from the tag and must equal the `[workspace.package]` version in `Cargo.toml`; the workflow fails with a clear message otherwise, before building anything.

1. Set the version in `Cargo.toml` under `[workspace.package]`, run `cargo build` once so `Cargo.lock` records it (the release builds use `--locked`), and in `CHANGELOG.md` rename `## [Unreleased]` to `## [X.Y.Z] - YYYY-MM-DD`, adding a fresh empty `Unreleased` section above it and the compare links at the bottom.
2. Commit, tag and push:

   ```sh
   git commit -am "release X.Y.Z"
   git tag -a vX.Y.Z -m "Warren X.Y.Z"
   git push origin main vX.Y.Z
   ```

3. The workflow builds `darwin-arm64` on `macos-14` (ad-hoc codesigned), `linux-x86_64` on `ubuntu-24.04` and `linux-arm64` on `ubuntu-24.04-arm` (both inside an `ubuntu:22.04` container for the glibc 2.35 floor, with the musl gateway), writes `SHA256SUMS`, takes the release notes from the `[X.Y.Z]` section of the changelog (else `[Unreleased]`, else the tag message), and publishes the GitHub Release with the tarballs, `SHA256SUMS` and a `warren.rb` formula already pointing at the new tarballs. A version with a `-` (`1.0.0-rc.1`) is marked a pre-release.
4. Update the Homebrew tap (below).

Dry runs: locally, `scripts/release/package.sh` builds and packages the current platform into `$CARGO_TARGET_DIR/dist/` with the same checks the workflow runs (version match, static gateway on Linux, signatures on macOS, the binaries report the version, the tarball lists exactly the expected files); `scripts/release/checksums.sh <dir>` and `scripts/release/notes.sh --version X.Y.Z` exercise the other steps. In GitHub, running the `Release` workflow by hand on a branch builds every platform and uploads a `warren-release-dry-run` artifact without creating a release; running it on a tag ref publishes.

### The Homebrew tap

The tap is a separate repository, `veerareddyvishal144/homebrew-warren`, holding `Formula/warren.rb`. Creating it, once:

```sh
gh repo create veerareddyvishal144/homebrew-warren --public --description "Homebrew tap for Warren"
git clone git@github.com:veerareddyvishal144/homebrew-warren.git
mkdir -p homebrew-warren/Formula
cp warren/packaging/homebrew/warren.rb homebrew-warren/Formula/warren.rb     # after the first release, see below
cd homebrew-warren && git add Formula && git commit -m "warren 0.1.0" && git push
```

Updating it for a release, from the Warren checkout:

```sh
gh release download vX.Y.Z -R veerareddyvishal144/warren -p SHA256SUMS -D /tmp/warren-rel
scripts/release/update-formula.sh --version X.Y.Z --sums /tmp/warren-rel/SHA256SUMS
cp packaging/homebrew/warren.rb ../homebrew-warren/Formula/warren.rb
```

`update-formula.sh` rewrites the `version`, the three `url` lines and their `sha256` lines from `SHA256SUMS`, and fails if a platform is missing. The release itself also carries a ready `warren.rb` asset (`gh release download vX.Y.Z -p warren.rb`) with the same content. Commit the updated formula in `packaging/homebrew/` to the Warren repository as well, so the two stay equal. Before pushing the tap, check it the way Homebrew does:

```sh
brew tap veerareddyvishal144/warren
brew audit --strict veerareddyvishal144/warren/warren
brew install veerareddyvishal144/warren/warren && brew test veerareddyvishal144/warren/warren
```
