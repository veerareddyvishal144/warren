# Agent API

Every instance runs `warren-gateway` on port 3737. On agent templates (`agent_api: true`) it serves this API: one endpoint to send a turn, server-sent events to stream it, sessions that persist on the instance's disk. It is the OpenAI "responses" shape reduced to what an agent in a box needs. Timestamps on this API are unix **milliseconds**.

The API is reached at the instance URL after edge auth, so the examples use the instance host and the workspace key. The daemon itself reaches the gateway by IP with no auth; only `/v1/exec` additionally requires the per-instance token. The files API ([files.md](files.md)) and exec ([exec.md](exec.md)) are served by the same gateway.

```bash
export WARREN_API_KEY=sk_live_...
KEY="X-Warren-Key: $WARREN_API_KEY"
JSON='Content-Type: application/json'
URL=http://ab12cd34ef.warren.localhost:7337     # the instance's `url`
```

## Health and version

```bash
curl -s $URL/v1/health -H "$KEY"      # {"healthy":true,"agent_ready":true,"driver":"openai","version":"0.1.0"}
curl -s $URL/v1/version -H "$KEY"     # {"version":"0.1.0"}
```

`driver` is `echo`, `openai`, `command`, or empty on a non-agent template; `agent_ready` reports whether the driver is ready to answer.

## Send a turn

```bash
curl -s -X POST $URL/v1/responses -H "$KEY" -H "$JSON" -d '{
  "input": "List the files in my home directory and summarise what this project does.",
  "session_id": null,
  "stream": false,
  "model": "default",
  "files": ["notes/brief.md"],
  "metadata": { "source": "docs" }
}'
```

| Field | Meaning |
|---|---|
| `input` | The user message. Required. |
| `session_id` | Continue this session; omit to start a new one. The response carries the id either way. |
| `stream` | `true` for server-sent events (below). |
| `model` | A model id from `GET /v1/models`; the driver's default when omitted. |
| `files` | Paths relative to the home directory the agent should look at. |
| `metadata` | Free-form JSON stored with the response. |

Non-streaming answer:

```json
{
  "id": "4f1c...", "session_id": "9a7b...", "status": "completed",
  "output_text": "Your home has plan.md, notes/ and a ttyd checkout. The project is ...",
  "created_at": 1763000000000, "completed_at": 1763000004200,
  "usage": { "input_tokens": 812, "output_tokens": 143 }
}
```

`status` is `completed` or `failed` (then `error` is set and `output_text` may be partial). `usage` is driver-dependent and optional. On a template without an agent the endpoint answers `404 not_supported`.

## Stream a turn

```bash
curl -sN -X POST $URL/v1/responses -H "$KEY" -H "$JSON" -d '{"input":"Write a haiku about containers.","stream":true}'
```

The body is `text/event-stream`; each event is `event: <name>` followed by `data: <json>` and a blank line:

```
event: response.created
data: {"id":"4f1c...","session_id":"9a7b..."}

event: response.output_text.delta
data: {"delta":"Thin walls of namespace,\n"}

event: response.tool_call
data: {"name":"run_shell","arguments":{"command":"ls ~"}}

event: response.output_text.delta
data: {"delta":"a kernel shared yet apart,\nsleep, then wake to serve."}

event: response.completed
data: {"id":"4f1c...","session_id":"9a7b...","output_text":"Thin walls of namespace,\na kernel shared yet apart,\nsleep, then wake to serve.","status":"completed"}
```

| Event | Data | When |
|---|---|---|
| `response.created` | `{ "id", "session_id" }` | First, as soon as the turn is accepted |
| `response.output_text.delta` | `{ "delta" }` | A piece of the answer; at least two per turn on the `echo` driver |
| `response.tool_call` | `{ "name", "arguments" }` | Optional; the `openai` driver emits one per tool it runs |
| `response.completed` | `{ "id", "session_id", "output_text", "status" }` | Last event of a successful turn, with the full text |
| `response.failed` | `{ "id", "error" }` | Last event of a failed turn |

## Fetch, replay, cancel

```bash
curl -s $URL/v1/responses/4f1c... -H "$KEY"                 # the response object
curl -sN $URL/v1/responses/4f1c.../stream -H "$KEY"         # replays stored events, follows a live turn, ends with response.completed
curl -s -X POST $URL/v1/responses/4f1c.../cancel -H "$KEY"  # {"id":"4f1c...","status":"cancelled"}
```

Replay is how a client that disconnected mid-stream catches up: stored events come first, then live ones.

## Sessions

```bash
curl -s $URL/v1/sessions -H "$KEY"
# {"data":[{"id":"9a7b...","name":"","created_at":1763000000000,"updated_at":1763000004200,"message_count":2}]}  newest first
curl -s $URL/v1/sessions/9a7b... -H "$KEY"
# {"id":"9a7b...","name":"","messages":[{"role":"user","content":"Write a haiku...","at":1763000000000},{"role":"assistant","content":"Thin walls...","at":1763000004200}]}
curl -s -X PATCH $URL/v1/sessions/9a7b... -H "$KEY" -H "$JSON" -d '{"name":"haikus"}'
curl -s -X DELETE $URL/v1/sessions/9a7b... -H "$KEY"
```

Session history is the message list the driver sends as context on the next turn. Sessions and responses persist as JSON files under `<home>/.warren/`, so they survive sleep, travel with forks and backups, and can be inspected or removed through the files API.

## Models

```bash
curl -s $URL/v1/models -H "$KEY"      # {"data":[{"id":"default"}, ...]}
```

`default` always exists; what else is listed depends on the driver.

## Drivers

The gateway picks its driver from `WARREN_AGENT_DRIVER`, normally set by the template and overridable per instance.

| Driver | Template | Behaviour | Configuration |
|---|---|---|---|
| `openai` | `warren-agent` | An agent loop against an OpenAI-compatible chat-completions endpoint with the tools `run_shell(command)`, `read_file(path)`, `write_file(path, content)`, `list_dir(path)`; at most 12 tool iterations per turn; the session's messages are the history; streams the final answer | `OPENAI_BASE_URL`, `OPENAI_API_KEY`, `WARREN_MODEL` in the instance env |
| `command` | `warren-opencode` and custom images | Runs `WARREN_AGENT_COMMAND` once per turn in the home directory with the prompt substituted for `{prompt}` (or appended as the last argument); stdout is the reply | `WARREN_AGENT_COMMAND`, plus whatever the command needs (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, ...) |
| `echo` | tests | Replies `echo: <input>`; an input starting with `!` runs the rest as a shell command and replies with its combined output (`!!sleep 3` style is allowed); streams in at least two deltas | none |

Set the credentials in the instance `env` at create time or with `PATCH` (applied at the next start). They never leave the instance and the workspace key is never inside it.

```bash
curl -s -X POST http://api.warren.localhost:7337/v1/instances -H "Authorization: Bearer $WARREN_API_KEY" -H "$JSON" -d '{
  "template": "warren-agent",
  "env": { "OPENAI_BASE_URL": "https://api.openai.com/v1", "OPENAI_API_KEY": "sk-...", "WARREN_MODEL": "gpt-4.1-mini" }
}'
```

## Sleeping instances

A request to the Agent API of a sleeping instance wakes it: the edge holds the connection until the gateway answers its health check, then forwards the request. Allow for that in client timeouts (tens of seconds on the first call). Crons rely on the same behaviour: each firing wakes the instance and posts `{"input": "<prompt>"}` with no session id ([crons.md](crons.md)).

## Errors

The gateway uses the same envelope as the Sandbox API: `{ "error": { "code", "message", "details" } }`. Expect `not_supported` (404) on non-agent templates, `not_found` for unknown response or session ids, `invalid_request` for a missing `input`, `forbidden` on `/v1/exec` without the instance token and on file paths outside the home.
