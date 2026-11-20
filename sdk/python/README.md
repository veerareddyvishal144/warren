# Warren Python client

`warren.py` is a one-file client for the Warren **Sandbox API** (instances,
templates, backups, crons) and the **Agent API** served inside every
instance (chat with streaming, sessions, files, exec). It needs only the
Python standard library (`urllib` for requests, `http.client` for streaming)
and Python 3.8 or newer.

Install by copying the file next to your code, or:

```bash
cp sdk/python/warren.py your-project/
```

## Quick start

```python
from warren import Warren, WarrenError

w = Warren("http://api.warren.localhost:7337", "sk_live_...")   # or WARREN_API_URL / WARREN_API_KEY

inst = w.create_instance(template="warren-agent", auto_sleep=True)
inst = w.wait_for(inst["id"], "running")            # create returns "provisioning" immediately
print(inst["url"])                                   # http://ab12cd34ef.warren.localhost:7337

result = w.exec(inst["id"], "uname -a")
print(result["exit_code"], result["stdout"])

agent = w.agent(inst)                                # Agent client for the instance URL, same key
for event, data in agent.stream("Summarise the files in my home directory"):
    if event == "response.output_text.delta":
        print(data["delta"], end="", flush=True)
    elif event == "response.completed":
        print("\n[done, session", data["session_id"] + "]")

agent.write_file("notes/todo.md", "- ship it\n")
print(agent.list_files("notes"))
```

No key yet? `Warren(api_url).signup(name="me")` returns a workspace, the
plaintext `api_key` (shown once) and its `key_id`.

## Authentication

* Sandbox API calls send `Authorization: Bearer <key>`.
* Calls to an instance URL send the same key as `x-warren-key`; the edge
  authenticates the request, wakes a sleeping instance and proxies it.
* `Agent.exec(token, ...)` additionally sends `x-warren-instance-token`; the
  token is the per-instance secret only the daemon knows (customers use
  `Warren.exec` instead).

## Errors

Every failure raises `WarrenError(code, message, status, details)`:

```python
try:
    w.start_instance("ab12cd34ef")
except WarrenError as e:
    if e.code == "instance_limit_reached":
        print(e.message, e.details)      # {"limit": 3, "awake": 3}
    elif e.code == "not_found":
        ...
    else:
        raise
```

`code` is one of `invalid_request`, `invalid_api_key`, `forbidden`,
`not_found`, `conflict`, `instance_limit_reached`,
`not_supported`, `try_again`, `timeout`, `bad_gateway`, `internal`. `status`
is the HTTP status, or 0 when the request never completed (connection refused,
socket timeout, client-side validation). A plain-text answer (for example the
edge's `502` for a port nothing listens on) is mapped from its status.

## Sandbox API (`class Warren`)

| Method | Endpoint |
|---|---|
| `health()`, `version()` | `GET /v1/health`, `GET /v1/version` |
| `signup(name=None)` | `POST /v1/signup` |
| `workspace()` | `GET /v1/workspace` (adds `awake_limit`, `None` = unlimited, and `awake_count`) |
| `list_api_keys()`, `create_api_key(name, expires_at)`, `delete_api_key(id)` | `/v1/api-keys` |
| `list_templates()`, `get_template(name)`, `register_template(name, image_ref, ...)`, `delete_template(name)` | `/v1/templates` |
| `create_instance(template, resources, type, region, name, metadata, env, auto_sleep, idle_timeout_seconds, network, allowed_domains, secrets)` | `POST /v1/instances` (`network="allowlist"` with `allowed_domains`; `secrets` are write-only `SecretSpec` dicts the host-side broker injects) |
| `list_instances()`, `get_instance(id)`, `patch_instance(id, ...)`, `delete_instance(id)` | `/v1/instances[/{id}]` |
| `stop_instance`, `start_instance`, `sleep_instance`, `restart_instance` | `POST /v1/instances/{id}/{stop,start,sleep,restart}` |
| `resize_instance(id, cpu, memory, disk)`, `update_instance(id, template)`, `fork_instance(id, ...)` | `POST .../{resize,update,fork}` |
| `exec(id, command, user, timeout_seconds, workdir, env)` | `POST /v1/instances/{id}/exec` |
| `logs(id, tail, since)`, `metrics(id, hours)`, `signed_url(id, port, ttl, path)` | `GET .../{logs,metrics,signed-url}` |
| `list_backups(id)`, `create_backup(id, note)`, `restore(id, backup_id)` | `.../backups`, `.../restore` |
| `create_cron(id, prompt, schedule, timezone, name, enabled)`, `list_crons`, `get_cron`, `patch_cron`, `delete_cron` | `.../crons[/{cid}]` |
| `wait_for(id, status="running", timeout=180, poll=1)` | polls `GET /v1/instances/{id}` |
| `instance_url(id, port=None)`, `agent(instance)` | helpers |

List endpoints return plain lists (the `{"data": [...]}` envelope is
unwrapped); everything else returns the JSON object as a `dict`. Timestamps
are unix seconds.

## Agent API (`class Agent`)

`Agent(instance_url, api_key)` talks to one instance; `Warren.agent(instance)`
builds it from an instance object, URL or id.

| Method | Endpoint |
|---|---|
| `health()`, `version()` | `GET /v1/health`, `GET /v1/version` |
| `respond(input, session_id, model, files, metadata)` | `POST /v1/responses` (waits for the turn) |
| `stream(input, session_id, model, files, metadata, timeout=None)` | `POST /v1/responses` with `stream: true`; generator of `(event, data)` |
| `get_response(id)`, `stream_response(id)`, `cancel(id)` | `/v1/responses/{id}[/stream|/cancel]` |
| `list_sessions()`, `get_session(id)`, `rename_session(id, name)`, `delete_session(id)` | `/v1/sessions[/{id}]` |
| `models()` | `GET /v1/models` |
| `list_files(path=".")`, `read_file(path)`, `read_text(path)`, `write_file(path, data, content_type)`, `delete_file(path)`, `move_file(src, dst)`, `download(path)` | `/v1/files`, `/v1/files/content`, `/v1/files/move`, `/v1/files/download` |
| `exec(token, command, user, timeout_seconds, workdir, env)` | `POST /v1/exec` with `x-warren-instance-token` |

Stream events, in order: `response.created` `{id, session_id}`,
`response.output_text.delta` `{delta}` (repeated), optional
`response.tool_call` `{name, arguments}`, then `response.completed`
`{id, session_id, output_text, status}` or `response.failed` `{id, error}`.
`data` is the decoded JSON object (or the raw string when the payload is not
JSON). Closing the generator early closes the connection. Agent API timestamps
are unix milliseconds.

Timeouts: `Warren(..., timeout=60.0)` and `Agent(..., timeout=600.0)` are
socket timeouts in seconds; `exec` extends them to cover the command's own
`timeout_seconds`; `stream(..., timeout=None)` waits as long as the agent
takes.

## Demo

```bash
WARREN_API_URL=http://api.warren.localhost:7337 WARREN_API_KEY=sk_live_... python3 warren.py
# optional: WARREN_INSTANCE_ID=ab12cd34ef WARREN_PROMPT="what time is it?"
```

Prints the daemon health, the workspace and its instances; with an instance id
it shows the agent health and streams one turn.

## Tests

Offline unit tests (urllib and http.client are mocked; nothing is contacted):

```bash
cd sdk/python && python3 -m unittest -v test_warren
```
