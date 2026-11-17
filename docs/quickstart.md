# Quickstart

This page takes a fresh Linux machine to a running instance with a shell, a browser terminal, a files API and an agent that answers `POST /v1/responses`. Everything is plain `curl`; the same calls are available from the `warren` CLI, the MCP server and the dashboard.

## 1. Prerequisites

- A Linux x86_64 host. Debian 12 and Ubuntu 22.04 or newer are what the images and scripts are tested with.
- Docker 24 or newer, and a user that can talk to it (`docker info` works without `sudo`, or run the daemon as root).
- A Rust toolchain, 1.90 or newer (`curl https://sh.rustup.rs -sSf | sh`), plus the musl target for the static gateway: `make musl-setup` (runs `sudo apt-get install musl-tools` and `rustup target add x86_64-unknown-linux-musl`).
- `git`, `curl`, and `jq` for the examples.

Optional, recommended: gVisor, so default instances run under `runsc` instead of `runc`:

```bash
sudo scripts/install-gvisor.sh
# downloads the release tarball, verifies its SHA-512, installs runsc, registers it in /etc/docker/daemon.json,
# reloads Docker and runs `docker run --rm --runtime=runsc alpine uname -r` as a smoke test
```

Without gVisor the daemon logs a warning and uses `runc`, which is fine for development and not recommended for untrusted code.

## 2. Build the images and start the daemon

```bash
git clone https://github.com/veerareddyvishal144/warren.git && cd warren
make musl-setup # one-time: musl-tools and the x86_64-unknown-linux-musl target
make images     # static musl build of warren-gateway, then docker build warren/base:dev, warren/agent:dev, warren/opencode:dev
make dev-up     # cargo build --release of the binaries, then warrend --dev on :7337 with data under ./data
```

`make dev-up` runs `scripts/dev-up.sh`; the equivalent direct command is:

```bash
target/release/warrend --dev --data-dir ./data
```

The script accepts `--no-build` and `WARREN_PORT`, `WARREN_DATA_DIR`, `WARREN_BASE_DOMAIN`, `WARREN_LOG` and `WARREN_IMAGE_TAG` in the environment; flags after `--` go to `warrend`. `--dev` lowers the minimum idle timeout to 5 seconds, lets instances reach the host at `host.docker.internal`, and keeps everything on `warren.localhost:7337`. The daemon prints `warrend listening` with the API URL when it is ready. Check it:

```bash
curl -s http://api.warren.localhost:7337/v1/health
# {"healthy":true,"version":"0.1.0","runtimes":["docker"],"dev_mode":true,"time":1763000000}
```

Browsers and hosts running systemd-resolved resolve `*.localhost` to the loopback address, so `api.warren.localhost`, `warren.localhost` and every instance host work without DNS changes. On a host without systemd-resolved, add `--resolve api.warren.localhost:7337:127.0.0.1` to curl (one entry per hostname) or put the names in `/etc/hosts`.

## 3. Sign up

```bash
export WARREN_API=http://api.warren.localhost:7337/v1
curl -s -X POST $WARREN_API/signup -H 'Content-Type: application/json' -d '{"name":"my workspace"}'
```

```json
{
  "workspace": { "id": "ws_k3j2h4g5f6d7s8a9q0w1", "name": "my workspace", "created_at": 1763000000 },
  "api_key": "sk_live_...",
  "key_id": "..."
}
```

The key is shown once. Keep it in a variable for the rest of this page:

```bash
export WARREN_API_KEY=sk_live_...
AUTH="Authorization: Bearer $WARREN_API_KEY"
```

## 4. Create an instance

```bash
curl -s -X POST $WARREN_API/instances -H "$AUTH" -H 'Content-Type: application/json' -d '{
  "template": "warren-base",
  "name": "first",
  "resources": { "cpu": 2, "memory": 4, "disk": 4 },
  "auto_sleep": true,
  "idle_timeout_seconds": 900
}'
```

The response is `201` with `"status": "provisioning"` and the instance's URL. Every field in the request is optional: the defaults are template `warren-base`, type `default`, the template's default shape (2 vCPU, 4 GB RAM, 4 GB disk), `auto_sleep: false`, `idle_timeout_seconds: 900`, `network: "allow-all"`.

Wait for `running`:

```bash
ID=$(curl -s $WARREN_API/instances -H "$AUTH" | jq -r '.data[0].id')
until [ "$(curl -s $WARREN_API/instances/$ID -H "$AUTH" | jq -r .status)" = running ]; do sleep 1; done
curl -s $WARREN_API/instances/$ID -H "$AUTH" | jq '{id, status, url, resources, runtime}'
```

## 5. Run commands

```bash
curl -s -X POST $WARREN_API/instances/$ID/exec -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"command": "cd ~ && git clone --depth 1 https://github.com/tsl0922/ttyd.git && ls ttyd | head"}'
```

```json
{ "exit_code": 0, "stdout": "CMakeLists.txt\nLICENSE\n...", "stderr": "Cloning into 'ttyd'...\n", "duration_ms": 1830 }
```

Commands run as the image user `agent` (uid 1000) in `/home/agent`; pass `"user": "root"` for root. Output is capped at 1 MiB per stream. See [exec.md](exec.md).

## 6. Use the instance URL

The instance URL serves whatever listens on the template's default port, which is the in-instance gateway (port 3737) for the built-in templates. The same API key works there:

```bash
curl -s http://$ID.warren.localhost:7337/v1/health -H "X-Warren-Key: $WARREN_API_KEY"
# {"healthy":true,"agent_ready":false,"driver":"","version":"0.1.0"}

# write and read a file in the persistent home
curl -s -X PUT "http://$ID.warren.localhost:7337/v1/files/content?path=notes/hello.txt" -H "X-Warren-Key: $WARREN_API_KEY" \
  --data-binary 'hello from the files API'
curl -s "http://$ID.warren.localhost:7337/v1/files?path=notes" -H "X-Warren-Key: $WARREN_API_KEY"
```

Any other port is reachable at `<id>-<port>`:

```bash
curl -s -X POST $WARREN_API/instances/$ID/exec -H "$AUTH" -H 'Content-Type: application/json' \
  -d '{"command": "cd ~ && nohup python3 -m http.server 8000 >/dev/null 2>&1 &"}'
curl -s http://$ID-8000.warren.localhost:7337/ -H "X-Warren-Key: $WARREN_API_KEY" | head -5
```

## 7. Open the browser terminal

Browsers cannot send the key header, so mint a signed URL for port 7681 (ttyd):

```bash
curl -s "$WARREN_API/instances/$ID/signed-url?port=7681&ttl=3600" -H "$AUTH"
# {"url":"http://ab12cd34ef-7681.warren.localhost:7337/?warren_token=...","expires_at":1763003600}
```

Open the `url` in a browser. The edge verifies the token, sets the `warren_session` cookie and redirects to the clean URL; you land in a login shell as `agent`. The dashboard at `http://warren.localhost:7337/` does the same behind its "terminal" button. See [urls.md](urls.md).

## 8. Talk to an agent

The `warren-agent` template runs the built-in agent against any OpenAI-compatible endpoint. Credentials go in the instance `env`; they are never baked into images and never leave the instance:

```bash
AGENT=$(curl -s -X POST $WARREN_API/instances -H "$AUTH" -H 'Content-Type: application/json' -d '{
  "template": "warren-agent",
  "name": "assistant",
  "auto_sleep": true,
  "env": { "OPENAI_BASE_URL": "https://api.openai.com/v1", "OPENAI_API_KEY": "sk-...", "WARREN_MODEL": "gpt-4.1-mini" }
}' | jq -r .id)
until [ "$(curl -s $WARREN_API/instances/$AGENT -H "$AUTH" | jq -r .status)" = running ]; do sleep 1; done

curl -s -X POST http://$AGENT.warren.localhost:7337/v1/responses -H "X-Warren-Key: $WARREN_API_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"input": "Create a file called plan.md in my home directory with three bullet points about Rust, then show it."}'
```

```json
{ "id": "...", "session_id": "...", "status": "completed", "output_text": "Done. plan.md contains: ...", "created_at": 1763000000000, "completed_at": 1763000004000 }
```

Pass the returned `session_id` in the next request to continue the conversation, or `"stream": true` for server-sent events. The `warren-opencode` template runs OpenCode behind the same API. See [agent-api.md](agent-api.md).

## 9. Sleep, wake, stop, delete

```bash
curl -s -X POST $WARREN_API/instances/$ID/sleep -H "$AUTH"      # {"id":"...","status":"sleeping"}: container stopped, disk kept
curl -s http://$ID.warren.localhost:7337/v1/health -H "X-Warren-Key: $WARREN_API_KEY"   # held while the instance wakes, then answered
curl -s -X POST $WARREN_API/instances/$ID/stop -H "$AUTH"       # {"id":"...","status":"stopping"}: never wakes on request
curl -s -X POST $WARREN_API/instances/$ID/start -H "$AUTH"      # {"id":"...","status":"starting"}
curl -s -X DELETE $WARREN_API/instances/$ID -H "$AUTH"          # {"id":"...","status":"deleting"}: disk removed
```

## 10. Check the awake quota

```bash
curl -s $WARREN_API/workspace -H "$AUTH" | jq '{awake_limit, awake_count}'
# {"awake_limit":null,"awake_count":1}      null: the daemon runs without --max-awake-instances
```

When the operator starts the daemon with `--max-awake-instances N`, create, start, wake and fork answer `409 instance_limit_reached` once `N` of the workspace's instances are awake; sleeping and stopped ones do not count. See [quotas.md](quotas.md).

## Next

- [concepts.md](concepts.md) for the vocabulary, [instances.md](instances.md) for every lifecycle action and the sizing table.
- [self-hosting.md](self-hosting.md) to run this on a real domain behind TLS.
- The CLI does all of the above: `warren signup`, `warren instances create --template warren-agent --env OPENAI_API_KEY=... --wait`, `warren instances exec <id> -- uname -a`, `warren chat <id> "hello"`, `warren instances open <id> --port 7681`.
