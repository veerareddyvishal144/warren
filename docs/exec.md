# Exec

Run a shell command inside an instance and get its exit code, stdout and stderr back in one JSON object.

```bash
export WARREN_API=http://api.warren.localhost:7337/v1
export WARREN_API_KEY=sk_live_...
AUTH="Authorization: Bearer $WARREN_API_KEY"
JSON='Content-Type: application/json'
ID=ab12cd34ef
```

## The call

```bash
curl -s -X POST $WARREN_API/instances/$ID/exec -H "$AUTH" -H "$JSON" -d '{
  "command": "cd ~/project && npm test 2>&1 | tail -20",
  "user": "",
  "timeout_seconds": 600,
  "workdir": "",
  "env": { "CI": "1" }
}'
```

| Field | Meaning | Default and limits |
|---|---|---|
| `command` | Run through a shell, so pipes, redirects, `&&` and globs work | required, non-empty, at most 1 MiB |
| `user` | `""` for the image user (`agent`, uid 1000, passwordless sudo), or `"root"` | image user; at most 32 characters |
| `timeout_seconds` | The command is killed when it passes | 900 (15 minutes); at most 86,400 |
| `workdir` | Working directory | the home directory (`/home/agent`) |
| `env` | Extra environment for this command only | same rules as instance env: keys `[A-Z_][A-Z0-9_]*`, at most 64 entries, values at most 4 KiB |

```json
{ "exit_code": 1, "stdout": "...\n  3 passing\n  1 failing\n", "stderr": "", "duration_ms": 8421, "truncated": false }
```

A non-zero `exit_code` is data, not an error: the HTTP status is still 200. Each stream is capped at 1 MiB; when a stream is cut `truncated` is `true`. Write long outputs to a file and fetch it with the files API instead.

## State rules

- A `sleeping` instance is woken first; the call is held until it is running, so the first exec after an idle period takes longer.
- A `stopped` instance answers `409 conflict`; start it first.
- Exec counts as activity and resets the idle timer.
- `provisioning`, `starting` and other transitions answer `409` or `503 try_again`; poll the status and retry.

## Background processes

Exec waits for the command to finish. To leave something running, detach it and redirect its output, otherwise the call waits for the pipes to close:

```bash
curl -s -X POST $WARREN_API/instances/$ID/exec -H "$AUTH" -H "$JSON" \
  -d '{"command":"cd ~/app && nohup node server.js > ~/app/server.log 2>&1 &"}'
curl -s http://$ID-3000.warren.localhost:7337/ -H "X-Warren-Key: $WARREN_API_KEY"
```

Processes do not survive sleep, stop or restart; only the home directory does. Start services from something the first request can trigger, or keep the instance awake (`auto_sleep: false`).

## Root and packages

```bash
curl -s -X POST $WARREN_API/instances/$ID/exec -H "$AUTH" -H "$JSON" \
  -d '{"command":"apt-get update && apt-get install -y --no-install-recommends ffmpeg","user":"root","timeout_seconds":900}'
```

Packages installed outside the home are lost when the container is recreated (sleep and wake, stop and start, update). Bake them into a template instead ([templates.md](templates.md)). `agent` has passwordless `sudo`, so `"user": "root"` and `sudo` inside the command are equivalent.

## How it runs

On `default` instances the daemon uses `docker exec`. On `vm` instances it calls the gateway's `POST /v1/exec` inside the microVM with the `X-Warren-Instance-Token` header; that token is generated per instance, injected as `WARREN_INSTANCE_TOKEN`, known only to the daemon and never exposed through the edge, so clients always go through the Sandbox API endpoint above. The gateway's exec endpoint accepts the same `ExecRequest` and returns the same `ExecResult`.

## CLI

```bash
warren instances exec ab12cd34ef -- 'uname -a && df -h ~'
warren instances exec ab12cd34ef --user root --timeout 60 -- apt-get install -y jq
```

The CLI exits with the command's exit code and prints stdout and stderr to the matching streams, so it composes with shell pipelines.

## Interactive terminals

Exec captures output when the command ends. For an interactive session use the gateway's `GET /v1/pty` WebSocket (a pseudo-terminal running a login shell or one command), which the CLI exposes as `warren shell <id>` and uses for `warren run -- <command>`; see [run.md](run.md) and the PTY subsection in [API.md](API.md).
