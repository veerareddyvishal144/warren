# warren-mcp: Warren as an MCP server

`warren-mcp` speaks the [Model Context Protocol](https://modelcontextprotocol.io) over stdio and gives any MCP client (Claude Code, Cursor, Claude Desktop, Windsurf, ...) tools to create and drive Warren instances: persistent Linux or macOS computers with a shell, a files API, an optional built-in agent, a browser terminal and, on macOS, a screen to look at and click on.

It is a thin layer over the Rust SDK (`warren-sdk`): every tool is one Sandbox API or Agent API call, and errors come back with the API's error code so the model can react (`instance_limit_reached`, `not_found`, `conflict`, ...).

## Build and install

```bash
source scripts/env.sh
cargo build --release -p warren-mcp
install -m 755 target/release/warren-mcp ~/.local/bin/
```

## Configuration

Two environment variables, read at start:

| Variable | Meaning | Default |
|---|---|---|
| `WARREN_API_URL` | Sandbox API base URL (with or without `/v1`) | `http://api.warren.localhost:7337/v1` |
| `WARREN_API_KEY` | Workspace API key (`sk_live_...`), required | none |

Get a key from a running daemon with `curl -X POST http://api.warren.localhost:7337/v1/signup` (dev) or `POST /v1/api-keys`.

The `warren` CLI writes all of the configurations below for you, with the URL and key from its own config file (`warren up` or `warren login` fill it): `warren connect claude`, `warren connect codex`, `warren connect cursor`, `warren connect windsurf`, `warren connect caret`, `warren connect opencode`, `warren connect gemini`. Add `--project` for the project-level file where the client has one, `--print` to see the entry instead of writing it; an existing file is merged, not replaced, and kept as `<file>.bak-<time>`. `warren skill install` adds a Claude Code skill that explains when and how to use the tools.

Logs go to stderr; set `RUST_LOG=debug` for request-level detail. Stdout is the protocol stream, so never pipe it anywhere else.

## Claude Code

```bash
claude mcp add warren \
  -e WARREN_API_URL=http://api.warren.localhost:7337 \
  -e WARREN_API_KEY=sk_live_your_key_here \
  -- warren-mcp
```

Add `--scope user` to make it available in every project, or `--scope project` to commit it to `.mcp.json` for the team (keep the key out of the repo and reference it as `${WARREN_API_KEY}` there). Check with `claude mcp list`, then ask Claude to "create a Warren instance and run `uname -a` in it".

## Cursor

Create `.cursor/mcp.json` in the project (or `~/.cursor/mcp.json` for all projects):

```json
{
  "mcpServers": {
    "warren": {
      "command": "warren-mcp",
      "env": {
        "WARREN_API_URL": "http://api.warren.localhost:7337",
        "WARREN_API_KEY": "sk_live_your_key_here"
      }
    }
  }
}
```

Cursor picks it up under Settings > MCP; enable the server and its tools appear in Agent mode. The same JSON block works for Claude Desktop (`claude_desktop_config.json`) and Windsurf.

## Tools

| Tool | Arguments | Returns |
|---|---|---|
| `list_instances` | none | `{ data: [instance...] }` newest first |
| `get_instance` | `id` | the instance object |
| `create_instance` | `template?`, `type?` (`default`/`vm`/`mac`), `cpu?`, `memory?`, `disk?`, `name?`, `auto_sleep?`, `idle_timeout_seconds?`, `env?` | the instance, after waiting up to 120 s for `running` (still returned, with a note, if it is not running yet) |
| `delete_instance` | `id` | `{ id, status: "deleting" }` |
| `stop_instance` | `id` | `{ id, status: "stopping" }` |
| `start_instance` | `id` | `{ id, status: "starting" }` |
| `sleep_instance` | `id` | `{ id, status: "sleeping" }` |
| `exec` | `id`, `command`, `user?`, `timeout_seconds?` | `{ exit_code, stdout, stderr, duration_ms, truncated }` |
| `read_file` | `id`, `path` | `{ path, size, truncated, encoding: "utf-8"/"base64", content }` |
| `write_file` | `id`, `path`, `content` | `{ path, size }` |
| `list_files` | `id`, `path?` (default `.`) | `{ path, entries: [{ name, type, size, modified_at }] }` |
| `chat` | `id`, `message`, `session_id?` | `{ output_text, session_id, response_id, status }` (agent templates only) |
| `signed_url` | `id`, `port?`, `ttl?` | `{ url, expires_at }` |
| `list_templates` | none | `{ data: [template...] }` |
| `screenshot` | `id` | an `image/png` content block plus `{ width, height, format, bytes }` (mac instances; others answer `not_supported`) |
| `click` | `id`, `x`, `y`, `button?` (`left`/`right`/`middle`), `count?` (1..3) | `{ id, done: [action] }` |
| `type_text` | `id`, `text` (`\n` is Return) | `{ id, done: [action] }` |
| `press_key` | `id`, `key` (`enter`, `escape`, `cmd+space`, `ctrl+c`, `f5`, ...) | `{ id, done: [action] }` |
| `scroll` | `id`, `x`, `y`, `dx?`, `dy?` (wheel steps, positive is right and down) | `{ id, done: [action] }` |

Every result is returned both as pretty-printed JSON text and as `structuredContent`. Paths are relative to the instance's persistent home directory. A non-zero `exit_code` from `exec` is data, not an error.

Failures are MCP tool errors (`isError: true`) whose text is `<code>: <message>`, with the full `{ "error": { code, message, details } }` object as structured content. Two codes come from this server rather than the API: `instance_failed` when a new instance ends in `failed` instead of `running`, and `response_failed` when the agent inside the instance reports a failed turn.

## Resource

`warren://instances` (`application/json`) returns the same listing as `list_instances`, for clients that attach resources as context.

## Development

```bash
source scripts/env.sh
cargo test -p warren-mcp
cargo clippy -p warren-mcp --all-targets -- -D warnings
```

The tests run against an in-memory fake backend; no daemon or Docker is needed. To try the server by hand, start the daemon (`make dev-up`), export the two variables and run `warren-mcp`, then paste an `initialize` request followed by `tools/list`, or point the MCP Inspector at it: `npx @modelcontextprotocol/inspector warren-mcp`.
