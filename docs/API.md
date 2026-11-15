# Warren API contract

This is the contract every component implements against: the daemon's **Sandbox API**, the per-instance **Agent API** served by the gateway inside each instance, and the **edge** rules that join them. Changes here are breaking changes.

## Planes and hosts

| Plane | Host | Auth | Served by |
|---|---|---|---|
| Sandbox API | `api.<base>` and `<base>` with path `/v1/...` | `Authorization: Bearer sk_live_...` (also accepts `X-Warren-Key`) | `warren-api` |
| Instance URLs | `<id>.<base>` (default port) and `<id>-<port>.<base>` (any port) | `X-Warren-Key: sk_live_...`, or a signed URL (`?warren_token=`) which sets the `warren_session` cookie, or `Authorization: Bearer` | `warren-edge` proxying to the instance |
| Dashboard | `<base>` root | browser, stores an API key locally | `warren-dashboard` |

`<base>` defaults to `warren.localhost` and URLs carry the public port, so locally: `http://api.warren.localhost:7337/v1/...`, `http://ab12cd34ef.warren.localhost:7337/`, `http://ab12cd34ef-7681.warren.localhost:7337/`.

Unauthenticated instance requests get `401 invalid_api_key`. A request to a sleeping instance is held while the instance wakes, then forwarded. A request to a stopped instance gets `409 conflict`. A port nothing listens on gets `502 bad_gateway` (a plain-text body is fine; the URL always routes).

CORS: the edge answers `OPTIONS` on instance hosts with `Access-Control-Allow-Origin: <request Origin>`, `Access-Control-Allow-Headers: X-Warren-Key, Content-Type, Authorization`, `Access-Control-Allow-Methods: GET, POST, PUT, PATCH, DELETE, OPTIONS`, `Access-Control-Allow-Credentials: true`, and adds `Access-Control-Allow-Origin` to proxied responses when an Origin is present. The API host does the same.

## Conventions

- Instance ids: 10 lowercase alphanumerics starting with a letter (`ab12cd34ef`); also the DNS label.
- Other ids: 32 hex characters. Workspace ids: `ws_` + 20 lowercase alphanumerics. API keys: `sk_live_` + 40 base62 characters; only the SHA-256 is stored.
- Money: integer micros (USD x 1,000,000) in fields ending `_micros`.
- Timestamps: unix seconds on the Sandbox API; the Agent API uses unix milliseconds.
- Lists: `{ "data": [...] }`, newest first.
- Errors: `{ "error": { "code": "...", "message": "...", "details": {...} } }`. Codes: `invalid_request` 400, `not_supported` 400, `invalid_api_key` 401, `insufficient_balance` 402, `forbidden` 403, `not_found` 404, `conflict` 409, `instance_limit_reached` 409, `try_again` 503, `timeout` 504, `internal` 500.
- Instance `status`: `provisioning | running | stopping | stopped | starting | restarting | updating | sleeping | waking | failed | deleting | deleted`.
- Unknown JSON fields are rejected with `invalid_request`.

## The instance object

```json
{
  "id": "ab12cd34ef", "workspace_id": "ws_...", "name": "Production agent",
  "template": "warren-agent", "template_name": "warren-agent", "template_revision": 1,
  "image_ref": "warren/agent:dev", "type": "default", "status": "running", "status_reason": "",
  "resources": { "cpu": 2, "memory": 4, "disk": 4 }, "region": "local",
  "metadata": { "plan": "pro" }, "env": { "APP_ENV": "production" },
  "network": "allow-all", "auto_sleep": true, "idle_timeout_seconds": 900,
  "budget": { "monthly_cap_micros": 0, "credit_micros": 0 },
  "url": "http://ab12cd34ef.warren.localhost:7337", "default_port": 3737, "mount_path": "/home/agent", "agent_api": true,
  "parent_id": "", "created_at": 1763000000, "updated_at": 1763000100, "started_at": 1763000050,
  "stopped_at": 0, "slept_at": 0, "last_activity_at": 1763000100, "meter_started_at": 1763000050,
  "runtime": "docker", "disk_used_bytes": 123456
}
```

`url` is derived from the base domain; `runtime` is `docker` or `firecracker`.

## Sandbox API (`/v1`)

### Health, version, signup
- `GET /v1/health` -> `{ "healthy": true, "version": "...", "runtimes": ["docker"], "time": 1763000000 }`. No auth.
- `GET /v1/version` -> `{ "version": "..." }`. No auth.
- `POST /v1/signup` body `{ "name"?: string }` -> `201 { "workspace": {...}, "api_key": "sk_live_...", "key_id": "...", "wallet_micros": 5000000 }`. No auth. Grants the configured free credit once. In non-dev mode may be disabled by config later; for now always on.

### Workspace, keys, billing
- `GET /v1/workspace` -> workspace object plus `{ "awake_limit": 3, "awake_count": 1 }`.
- `GET /v1/api-keys` -> list of keys (no hashes). `POST /v1/api-keys` body `{ "name"?: string, "expires_at"?: int }` -> `201 { "key": {...}, "api_key": "sk_live_..." }` (plaintext shown once). `DELETE /v1/api-keys/{id}` -> `{ "id": "...", "deleted": true }`. Deleting the key used for the request is allowed.
- `GET /v1/billing` -> `{ "wallet_micros": ..., "top_up_total_micros": ..., "awake_limit": 3, "awake_count": 0, "rates": {...}, "free_credit_granted": true }`.
- `POST /v1/billing/top-up` body `{ "amount_micros": int, "provider"?: "dev" }` -> `{ "workspace": {...}, "entry": {...} }`. Only `provider: "dev"` exists in this build and it is accepted only when the daemon runs with `--dev`; otherwise `not_supported`. Amount between 5,000,000 and 10,000,000,000.
- `GET /v1/billing/ledger?limit=100` -> list of ledger entries.

Awake-instance limit by cumulative top-ups: 3 before any top-up; 10 after the first; 50 at >= $50; 200 at >= $250; 1000 at >= $1,000. Sleeping and stopped instances never count.

### Templates
- `GET /v1/templates` -> system catalog first, then workspace templates by name.
- `GET /v1/templates/{name}` -> one template; `name@<revision>` for a pinned workspace revision; `404` if unknown or unpublished.
- `POST /v1/templates` body `RegisterTemplateRequest` (`name` lowercase `[a-z0-9-]{3,40}`, not starting with `warren-`; `image_ref` required) -> `201` template with `revision: 1`. The image is pulled and its digest recorded. Posting again with the same name publishes a new revision.
- `DELETE /v1/templates/{name}` -> `{ "name": "...", "deleted": true }`; `409 conflict` if instances use it.

### Instances
- `POST /v1/instances` body `CreateInstanceRequest` (all optional; default template `warren-base`, type `default`, `auto_sleep: false`, `idle_timeout_seconds: 900`, `network: "allow-all"`). Validation: template resolves; type allowed by template and a runtime exists for it (`performance` -> `not_supported`); shape per `warren_core::shapes::normalize`; `idle_timeout_seconds` within `[MinIdleTimeoutSec, 86400]`; env keys `[A-Z_][A-Z0-9_]*`, at most 64 entries, values <= 4 KiB; metadata <= 32 entries. Billing: wallet must hold one day of the running rate -> `402 insufficient_balance`; awake limit -> `409 instance_limit_reached`. Returns `201` with status `provisioning` immediately. Nothing is charged until the instance first reaches `running`.
- `GET /v1/instances` -> list. `GET /v1/instances/{id}` -> instance. `PATCH /v1/instances/{id}` body `PatchInstanceRequest` -> instance (auto-sleep, idle timeout, name, metadata, env, network, budget change live; env changes apply at next start).
- `DELETE /v1/instances/{id}` -> `{ "id": "...", "status": "deleting" }`; the instance is destroyed and removed, final minutes settled.

Lifecycle operations below return `{ "id", "status" }`. In the single-node build they complete synchronously, so the status is the resulting state: `stop` answers `stopped`, `start` and `restart` answer `running`, `sleep` answers `sleeping`, `update` answers `running`; clients must still accept the transitional states (`stopping`, `starting`, `restarting`, `updating`) that a multi-node build may return.
- `POST /v1/instances/{id}/stop` -> `{ "id", "status": "stopping" }`. Stopped bills disk only and never wakes on request.
- `POST /v1/instances/{id}/start` -> `{ "id", "status": "starting" }` (from stopped or sleeping). Enforces the awake limit.
- `POST /v1/instances/{id}/sleep` -> `{ "id", "status": "sleeping" }` now, regardless of auto-sleep. In this build sleep stops the container and keeps the disk; any request to the instance URL wakes it (fresh boot, files intact). Memory checkpoints are a roadmap item.
- `POST /v1/instances/{id}/restart` -> `{ "id", "status": "restarting" }`.
- `POST /v1/instances/{id}/resize` body `{ "cpu"?, "memory"?, "disk"? }` -> `{ "id", "status", "resources" }`. Disk may only grow. The runtime applies live when it can, otherwise stop/apply/start.
- `POST /v1/instances/{id}/update` body `{ "template"?: "name" | "name@rev" }` -> `{ "id", "status": "updating", "image_ref", "template_revision" }`. Replaces the image, keeps `mount_path` data.
- `POST /v1/instances/{id}/fork` body `ForkRequest` -> `201` new instance (copy of disk, same template and env, new id and URL). Fork requires the source to be stopped or sleeping; a running source is slept first and woken after the copy.
- `POST /v1/instances/{id}/exec` body `ExecRequest` -> `ExecResult`. Default user is the image user; `"root"` allowed. Wakes a sleeping instance; `409` if stopped. Output capped at 1 MiB per stream with `truncated: true`.
- `GET /v1/instances/{id}/logs?tail=200&since=<unix>` -> `{ "logs": "..." }`.
- `GET /v1/instances/{id}/metrics?hours=1` -> `{ "data": [MetricPoint...] }` sampled every minute while running.
- `GET /v1/instances/{id}/signed-url?port=7681&ttl=3600&path=/` -> `{ "url": "http://ab12cd34ef-7681.<base>/?warren_token=...", "expires_at": ... }`. `port` omitted means the default port. TTL 60..86400.
- Backups: `GET /v1/instances/{id}/backups` list; `POST /v1/instances/{id}/backups` body `{ "note"? }` -> `201` backup (taken without stopping); `POST /v1/instances/{id}/restore` body `{ "backup_id" }` -> instance (stopped, restored, restarted if it was running).
- Crons: `POST /v1/instances/{id}/crons` body `{ "prompt", "schedule", "timezone"?, "name"?, "enabled"?: true }` -> `201` cron. `GET .../crons`, `GET .../crons/{cid}`, `PATCH .../crons/{cid}`, `DELETE .../crons/{cid}`. Only on agent templates (`agent_api: true`), else `not_supported`. Each firing wakes the instance and posts `{"input": prompt}` to the gateway's `/v1/responses` with no session id. `schedule` is a classic five-field expression: minute, hour, day-of-month, month, day-of-week, with day-of-week `0-7` where `0` and `7` are Sunday and names `SUN`..`SAT` accepted; shorthands such as `@daily` are rejected. The server translates it with `warren_core::cronexpr::to_cron_crate` before parsing.

## Agent API (served by the gateway inside the instance on port 3737)

Reachable at the instance URL after edge auth. The daemon reaches it directly by IP with no auth; `/v1/exec` additionally requires `X-Warren-Instance-Token`.

- `GET /v1/health` -> `{ "healthy": true, "agent_ready": true, "driver": "openai", "version": "..." }`.
- `GET /v1/version` -> `{ "version": "..." }`.
- `POST /v1/responses` body `{ "input": string, "session_id"?: string, "stream"?: bool, "model"?: string, "files"?: [string], "metadata"?: object }`.
  - Non-stream -> `{ "id", "session_id", "status": "completed" | "failed", "output_text", "created_at", "completed_at", "usage"?: {...}, "error"?: string }`.
  - Stream -> `text/event-stream` with named events: `response.created` `{id, session_id}`, `response.output_text.delta` `{delta}`, `response.tool_call` `{name, arguments}` (optional), `response.completed` `{id, session_id, output_text, status}`, `response.failed` `{id, error}`. Each event is `event: <name>\ndata: <json>\n\n`.
  - On a non-agent template -> `404 not_supported`.
- `GET /v1/responses/{id}` -> the response object. `GET /v1/responses/{id}/stream` -> replays stored events then follows a live one; ends with `response.completed`.
- `POST /v1/responses/{id}/cancel` -> `{ "id", "status": "cancelled" }`.
- `GET /v1/sessions` -> `{ "data": [{ "id", "name", "created_at", "updated_at", "message_count" }] }` newest first. `GET /v1/sessions/{id}` -> `{ "id", "name", "messages": [{ "role", "content", "at" }] }`. `PATCH /v1/sessions/{id}` body `{ "name" }`. `DELETE /v1/sessions/{id}`.
- `GET /v1/models` -> `{ "data": [{ "id": "default" }, ...] }`.
- Files, all paths relative to the persistent home and confined to it (`..` and absolute paths outside home -> `403 forbidden`): `GET /v1/files?path=.` -> `{ "path", "entries": [{ "name", "type": "file"|"dir", "size", "modified_at" }] }`; `GET /v1/files/content?path=` -> raw bytes with a content type guess; `PUT /v1/files/content?path=` raw body -> `{ "path", "size" }` (creates parents); `DELETE /v1/files?path=` -> `{ "path", "deleted": true }`; `POST /v1/files/move` body `{ "from", "to" }`; `GET /v1/files/download?path=` -> `application/gzip` tarball of a directory.
- `POST /v1/exec` header `X-Warren-Instance-Token` body `ExecRequest` -> `ExecResult`. Used by the daemon for vm-type instances; the docker runtime uses Docker exec instead.

Agent drivers (selected by `WARREN_AGENT_DRIVER`):
- `echo`: deterministic for tests. Replies `echo: <input>`; an input starting with `!` runs the rest as a shell command and replies with its combined output; `!!sleep N` style commands are allowed. Streams the reply in at least two deltas.
- `openai`: an agent loop against an OpenAI-compatible chat-completions endpoint (`OPENAI_BASE_URL`, `OPENAI_API_KEY`, model from `WARREN_MODEL`) with tools `run_shell(command)`, `read_file(path)`, `write_file(path, content)`, `list_dir(path)`; at most 12 tool iterations per turn; session history is the message list; streams the final answer.
- `command`: runs `WARREN_AGENT_COMMAND` once per turn with the prompt as the last argument (`{prompt}` placeholder supported) in the home directory; stdout is the reply.

Sessions and responses persist as JSON files under `<home>/.warren/`.

## Edge rules

1. Parse Host with `Config::parse_host`. `root` -> dashboard, except paths under `/v1/` which go to the API. `api` -> API.
2. `instance`: resolve auth: `X-Warren-Key` or `Authorization: Bearer` -> `Authenticator`; else `?warren_token=` -> `Signer.Verify` (id and port must match the host; port 0 in the token means the default port) -> set cookie `warren_session=<token>` (HttpOnly, SameSite=Lax, Path=/) and `302` to the same URL without the token; else cookie `warren_session` -> verify likewise. Failures -> `401 invalid_api_key` JSON.
3. The instance must belong to the authenticated workspace (key auth) -> else `404 not_found`.
4. `Lifecycle.EnsureAwake(id)`; `Lifecycle.RecordActivity(id)` on every proxied request and response.
5. Proxy to `http://<ip>:<port>` with `httputil.ReverseProxy`, preserving path and query, setting `X-Forwarded-Host`, `X-Forwarded-Proto`, stripping `X-Warren-Key`, `Authorization` is preserved for the instance's own use, `warren_session` cookie stripped from the forwarded Cookie header. WebSocket upgrades pass through.
6. Port: 0 -> `Instance.DefaultPort`; if the default port is 0 -> `502` with text "this template has no default port".
