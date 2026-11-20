# Quickstart

From an installed `warren` to a running instance with a coding agent inside, on an Apple silicon Mac or a Linux host. Everything here is the `warren` CLI; the same calls exist as HTTP ([API.md](API.md), with a curl walkthrough in [TESTING.md](TESTING.md)), in the MCP server, the SDKs and the dashboard.

## 1. Install

[install.md](install.md) has every method; the short version:

```sh
brew install veerareddyvishal144/warren/warren                                      # macOS or Linux, Homebrew
curl -fsSL https://raw.githubusercontent.com/veerareddyvishal144/warren/main/scripts/install.sh | sh   # or the install script
warren --version
```

Both put `warren`, `warrend`, `warren-mcp` and `warren-gateway` side by side, which is where `warren up` and `warren connect` look for them. From a checkout, `cargo build --release -p warrend -p warren-cli -p warren-mcp -p warren-gateway` does the same into `target/release/`.

What the host needs:

- **macOS**: an Apple silicon Mac running macOS 14 or newer, [Lume](https://github.com/trycua/cua) 0.7.2 or newer on `PATH` (`/bin/bash -c "$(curl -fsSL https://cua.ai/lume/install.sh)"`; `warren up` prints this when Lume is missing), and about 30 GB of free disk for the base VM. Docker Desktop is optional: with it, the same daemon also serves Linux `default` instances.
- **Linux**: Docker 24 or newer that your user can talk to (`docker info` without `sudo`; otherwise `sudo usermod -aG docker $USER` and log in again, or run the daemon as root with the system unit from [install.md](install.md)). gVisor is recommended so `default` instances run under `runsc` rather than `runc`: `sudo scripts/install-gvisor.sh` from a checkout. The template images are built locally once, from a checkout, with the installed gateway: `WARREN_GATEWAY_BIN=$(command -v warren-gateway) scripts/build-images.sh` (builds `warren/base:dev`, `warren/agent:dev`, `warren/opencode:dev`, `warren/claude:dev`).

`warren doctor` checks all of this at any point and prints a fix for whatever is missing.

## 2. Start the daemon: `warren up`

```sh
warren up
```

```text
Warren is up.
  dashboard  http://warren.localhost:7337/
  API        http://api.warren.localhost:7337
  key        /Users/alice/.config/warren/config.json (sk_live_k3j2h4..., new workspace)
  data       /Users/alice/Library/Application Support/Warren
  service    launchd dev.warren.daemon (pid 4242)
  log        /Users/alice/Library/Logs/Warren/warrend.log
  daemon     warrend 0.1.0, runtimes: lume
Next:
  warren connect claude    # register the MCP server with Claude Code (codex, cursor, windsurf, caret, opencode, gemini too)
  warren run --type mac -- claude    # Claude Code in a fresh macOS instance bound to this directory
  warren doctor            # check the setup
```

`warren up` finds `warrend` next to itself, picks the runtime (`--runtime auto`: Docker when `docker info` works, nothing but Lume on a Mac without it; `none` or `docker` to force), refuses a port that something else holds and names the process, and installs the daemon as a service that starts at login:

- macOS: a launchd LaunchAgent, `dev.warren.daemon`, rendered from `packaging/launchd/dev.warren.daemon.plist` with absolute paths, running `warrend --runtime none --mac-base-vm warren-macos-26 --data-dir "~/Library/Application Support/Warren" --listen 127.0.0.1:7337`; log in `~/Library/Logs/Warren/warrend.log`.
- Linux: a systemd user unit, `warrend`, rendered from `packaging/systemd/warrend.service` for user mode (`systemctl --user`), data under `$XDG_DATA_HOME/warren` (`~/.local/share/warren`); `journalctl --user -u warrend` for the log. When your user cannot talk to Docker, `warren up` writes the system unit and the environment file for you and prints the `sudo install ... && sudo systemctl enable --now warrend` steps instead.

Then it waits up to 120 s for `GET /v1/health`, signs up a workspace when the config has no key for that URL, saves `api_url` and `api_key` to `~/.config/warren/config.json` (mode 600), and prints the dashboard and API URLs. Run it again at any time: it stops the previous daemon and installs the new settings. Useful flags: `--port N` (default 7337), `--data-dir DIR`, `--base-domain D` (default `warren.localhost`), `--mac-base-vm NAME`, `--dev` (5 s idle timeouts, instances can reach the host), `--no-service` (a plain background process with its pid in `<data>/warrend.pid`, for a terminal session or a test), `--warrend-bin PATH`.

Three companions:

```sh
warren status            # service state, health, runtimes, instance count
warren doctor            # OK/WARN/FAIL per check with a fix each; exit 1 on a failure
warren down              # stop the daemon; --purge also removes the service and, after confirmation, the data
```

On macOS the daemon's `runtimes` line says `lume` once the base VM exists; until then it says `none` and `warren up` points you at the next step.

## 3. macOS: build the base VM: `warren mac init`

Every `mac` instance is an APFS clone of one base VM, built once:

```sh
warren mac init                     # 20 to 40 minutes the first time, mostly downloads
warren mac init --with-agents       # also Node LTS and the claude, codex and opencode CLIs in the image
warren mac init --dry-run           # print every step and command, run nothing
```

Without `--source` it first creates a stock VM named `base` from Apple's current restore image (`lume create base --os macos --cpu 4 --memory 4GB --disk-size 50GB --ipsw latest --unattended tahoe`; `--ipsw PATH` for an image you already have, `--unattended sequoia` for a macOS 15 image): Lume's offline setup creates the user `lume`, turns SSH on and boots once to verify. Then it runs `scripts/mac/build-base.sh` (embedded in the binary) to clone `base` into `warren-macos-26`, install the gateway launcher and the `pf` isolation rules, put your SSH key (`~/.ssh/id_ed25519`, generated when missing) in the guest, replace the stock password with a random one kept in `~/.warren/mac/warren-macos-26.password`, and stop the VM. `--source NAME` uses a stock VM you already have, `--target NAME` names the result, `--force` replaces an existing target.

When it is done, `warren up` once more so the daemon picks the VM up; `warren status` then reports `runtimes: lume`. A Mac runs at most two macOS guests at a time ([macos.md](macos.md) has the details and the design); Linux `default` instances on the same Mac need Docker Desktop and `warren up --runtime docker`.

## 4. Connect a coding agent: `warren connect`

```sh
warren connect claude       # Claude Code: runs `claude mcp add warren --scope user ...`
warren connect codex        # Codex: ~/.codex/config.toml
warren connect cursor       # Cursor: ~/.cursor/mcp.json (--project: .cursor/mcp.json)
warren connect windsurf     # Windsurf: ~/.codeium/windsurf/mcp_config.json
warren connect caret        # Caret: ~/.caret/mcp.json
warren connect opencode     # OpenCode: ~/.config/opencode/opencode.json
warren connect gemini       # Gemini CLI: ~/.gemini/settings.json
```

Each one registers the `warren-mcp` server (installed next to `warren`) with `WARREN_API_URL` and `WARREN_API_KEY` from your config, merges into an existing file without touching other servers, keeps the previous file as `<file>.bak-<time>`, and prints the result with `--print` instead of writing. Restart the agent and ask it to "create a Warren instance and run `uname -a` in it"; the tools are listed in [mcp/README.md](../mcp/README.md). For Claude Code, `warren skill install` adds a skill that explains when to reach for a sandbox and how to use one ([for-agents.md](for-agents.md)).

## 5. The first instance

```sh
warren instances create --template warren-base --wait          # Linux computer (Debian 12, Node, Python, git); --type vm for a Firecracker microVM
warren instances create --template warren-macos --wait         # macOS VM on a Mac (type mac is implied)
warren instances list
```

The create prints the instance id (10 characters) and waits for `running`. Then:

```sh
warren instances exec ab12cd34ef -- uname -a                   # a command with captured output; the exit code is yours
warren shell ab12cd34ef                                        # an interactive terminal inside
warren files put ab12cd34ef ./notes.md notes/notes.md          # files in the persistent home
warren files ls ab12cd34ef notes
warren instances exec ab12cd34ef -- 'cd ~ && nohup python3 -m http.server 8000 >/dev/null 2>&1 &'
warren instances url ab12cd34ef --port 8000                    # a signed URL for anything listening inside
warren instances open ab12cd34ef --port 7681                   # the browser terminal (Linux templates)
```

Commands run as `agent` in `/home/agent` on Linux (passwordless sudo; `--user root` for root) and as `lume` in `/Users/lume` on macOS. Anything listening on a port inside is reachable at `http://<id>-<port>.warren.localhost:7337/` with the key in `X-Warren-Key`, or through a signed URL a browser can open. The dashboard at `http://warren.localhost:7337/` shows the same instances with a terminal button; log in with the key from `~/.config/warren/config.json`. Shapes are `--cpu`/`--memory` 2/4, 4/8, 4/16, 4/32; `--env KEY=value` passes credentials for tools inside the instance (never the Warren key itself); `--auto-sleep` lets an idle instance sleep. [instances.md](instances.md) has every action.

## 6. A coding agent inside: `warren run`

```sh
cd ~/code/my-app
warren run --template warren-claude -- claude    # Linux: the warren-claude image has Claude Code, Codex and OpenCode installed
warren run --type mac -- claude                  # macOS VM built with `warren mac init --with-agents`
warren run -- npm test                           # any command, in the default warren-base image
```

`warren run` creates an instance bound to the current directory (remembered in `.warren/instance.json`, reused next time; `--new` for a fresh one; `--template` and `--type` only matter when it creates), pushes the directory honouring `.gitignore`, runs the command on a terminal inside under the mirrored path, and pulls back the files that changed when it exits. `--no-sync` skips the copying, `--no-tty` runs through exec and prints the captured output, `--remote-root PATH` changes the remote directory, `warren run --unbind [--delete]` forgets (and optionally destroys) the bound instance; `warren sync push` and `warren sync pull` move files by hand. Agent CLIs inside the instance sign in the way they do anywhere else (a browser login on the instance URL, or provider keys set in the instance's environment).

## 7. Sleep, wake, stop, delete

```sh
warren instances sleep ab12cd34ef      # processes stop, the disk stays; any request wakes it (mac: memory kept, processes resume)
warren instances stop ab12cd34ef       # never wakes on request
warren instances start ab12cd34ef
warren instances fork ab12cd34ef --name copy
warren instances delete ab12cd34ef     # the disk goes too
```

Files in the home survive sleep, stop and restart; processes and packages installed outside the home do not, except across a macOS sleep. `warren whoami` shows the workspace's awake count and limit when the operator set one ([quotas.md](quotas.md)).

## 8. Where things are, and how to remove them

| | macOS | Linux |
|---|---|---|
| Config (URL, key) | `~/.config/warren/config.json` | same |
| What `warren up` set up | `~/.config/warren/daemon.json` | same |
| Data: SQLite, secret, instance disks, backups | `~/Library/Application Support/Warren` | `~/.local/share/warren` |
| Service | `~/Library/LaunchAgents/dev.warren.daemon.plist` | `~/.config/systemd/user/warrend.service` |
| Log | `~/Library/Logs/Warren/warrend.log` | `journalctl --user -u warrend` |
| Base VM, guest password | `~/.lume/warren-macos-26`, `~/.warren/mac/warren-macos-26.password` | |

`warren down --purge` stops the daemon, removes the service and, after a confirmation (`--yes` to skip it), the data directory; the config file and the base VM stay (`lume delete warren-macos-26` removes the VM). [install.md](install.md) covers removing the binaries.

## Next

- [concepts.md](concepts.md) for the vocabulary, [instances.md](instances.md) for every lifecycle action and the sizing table, [templates.md](templates.md) for the catalog and your own images.
- [macos.md](macos.md) for what a `mac` instance is and is not, [limits.md](limits.md) for the hard limits by type.
- [self-hosting.md](self-hosting.md) to run the daemon on a server with a real domain and TLS, as root with the system unit.
- [for-agents.md](for-agents.md) and [mcp/README.md](../mcp/README.md) for agents, [API.md](API.md) for the wire contract behind every command (`--json` on any command prints the raw response).
