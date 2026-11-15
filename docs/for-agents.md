# Warren for coding agents

Two ways to make a coding agent (Claude Code, Cursor, Codex, OpenCode, your own) productive with Warren: paste the prompt below, or point it at `docs/llms.txt`. For MCP-capable agents there is also a server that exposes instances as tools ([mcp/README.md](../mcp/README.md)).

## The prompt

Copy this block into the agent's instructions (a `CLAUDE.md`, `.cursor/rules`, `AGENTS.md`, or the chat). Replace the two placeholders.

````text
You can create and use Warren instances: persistent Linux computers (Debian 12, Node 24, Python 3, git, build tools) reachable over HTTP.

Environment
- Sandbox API base URL: http://api.warren.localhost:7337/v1   (replace with the real one)
- API key: sk_live_...                                        (replace; never print it or write it to files)
- Auth on the Sandbox API: `Authorization: Bearer <key>`. Auth on instance URLs: `X-Warren-Key: <key>`.
- Every response body is JSON; lists are `{"data":[...]}`; errors are `{"error":{"code","message","details"}}`.
  Codes: invalid_request 400, not_supported 400, invalid_api_key 401, insufficient_balance 402, forbidden 403,
  not_found 404, conflict 409, instance_limit_reached 409, try_again 503, timeout 504. Unknown JSON fields are rejected.

Instances
- Create: POST /instances with any of {"template":"warren-base"|"warren-agent"|"warren-opencode", "name", "resources":{"cpu","memory","disk"},
  "env":{...}, "auto_sleep":true, "idle_timeout_seconds":900, "network":"allow-all"|"deny-all"}. Shapes: 2/4, 4/8, 4/16, 4/32 (vCPU/GB).
  The response has "id" (10 chars) and "url". Status starts at "provisioning"; poll GET /instances/{id} until "running" before using it.
- Run a command: POST /instances/{id}/exec {"command":"...", "user":"root"?, "timeout_seconds":600?} -> {"exit_code","stdout","stderr","duration_ms","truncated"}.
  The command runs through a shell as user agent (uid 1000, passwordless sudo) in /home/agent. A non-zero exit_code is normal data. Output is capped at 1 MiB per stream.
- Files (on the instance URL, paths relative to /home/agent): GET /v1/files?path=.  GET /v1/files/content?path=f  PUT /v1/files/content?path=f (raw body)
  DELETE /v1/files?path=f  POST /v1/files/move {"from","to"}  GET /v1/files/download?path=dir (tar.gz).
- Anything listening on port P inside the instance is reachable at http://{id}-P.<base>/ with the X-Warren-Key header.
  For a browser or a human: GET /instances/{id}/signed-url?port=P&ttl=3600 -> {"url"}; share that URL instead of the key.
- Lifecycle: POST /instances/{id}/stop | start | sleep | restart; DELETE /instances/{id}. Sleeping instances wake on any request and bill disk only;
  stopped ones answer 409 until started. Processes and packages outside /home/agent are lost on sleep, stop and restart; files in /home/agent persist.
- Agent templates (warren-agent, warren-opencode) answer POST {url}/v1/responses {"input":"...", "session_id"?, "stream"?} with
  {"id","session_id","status","output_text"}; pass session_id back to continue a conversation.

Money and limits
- Prepaid wallet in micro-dollars (1,000,000 = $1); signup grants $5. A 2 vCPU / 4 GB / 4 GB instance costs $4.76 a month running, $0.36 sleeping.
  Creating needs one day of the running rate in the wallet. At most 3 instances may be awake before any top-up.
- Prefer one instance per task, "auto_sleep": true, and DELETE instances you no longer need. Check GET /billing when you get 402 or 409 instance_limit_reached.

Working style
- Do the work inside the instance with exec; keep outputs under /home/agent and read them back with the files API.
- Wait for "running" after create, start, or update; retry on 503 try_again after a few seconds.
- Never put the API key in instance env, files, prompts or logs. Credentials for tools inside the instance go in the instance "env" at create time.
````

If the agent writes Python or TypeScript, point it at the one-file clients in [sdk/python](../sdk/python/README.md) and [sdk/typescript](../sdk/typescript/README.md) (`Warren(...).create_instance(...)`, `wait_for(id, "running")`, exec, files and streaming chat) instead of hand-written HTTP. For the CLI instead of raw HTTP, append: "The `warren` CLI wraps the same API: `warren instances create --template warren-base --wait`, `warren instances exec <id> -- <command>`, `warren instances url <id> --port 8000`, `warren instances delete <id>`; add `--json` for machine-readable output."

## The llms.txt approach

[`docs/llms.txt`](llms.txt) is an index of every documentation page with a one-line description, in the `llms.txt` convention. Give the agent that file and let it fetch what it needs:

```text
Read https://raw.githubusercontent.com/veerareddyvishal144/warren/main/docs/llms.txt, then fetch the pages you need before using the API.
```

In a checkout, the same works with paths: "Read docs/llms.txt and the pages it links before you start; docs/API.md is the contract." Keep `docs/API.md` as the final word when a page and the contract disagree.

For a self-hosted Warren, serve `docs/llms.txt` next to your dashboard or keep it in the agent's repository; the pages are plain Markdown with no build step.

## MCP

Agents that speak MCP can skip HTTP entirely:

```bash
cargo build --release -p warren-mcp && install -m 755 target/release/mcp ~/.local/bin/warren-mcp
claude mcp add warren -e WARREN_API_URL=http://api.warren.localhost:7337 -e WARREN_API_KEY=sk_live_... -- warren-mcp
```

Tools: `create_instance`, `exec`, `read_file`, `write_file`, `list_files`, `chat`, `signed_url`, `stop_instance`, `start_instance`, `sleep_instance`, `delete_instance`, `list_instances`, `get_instance`, `list_templates`, `billing`. See [mcp/README.md](../mcp/README.md) for Cursor, Claude Desktop and Windsurf.

## What agents get wrong

- Using the instance before it is `running`: poll the status.
- Forgetting that sleep and restart kill processes: write results to files, start servers from the request that needs them.
- Installing packages with `apt` and expecting them after a sleep: bake them into a template ([templates.md](templates.md)) or reinstall at the start of each task.
- Sending `-d` with multi-line file content to the files API: use `--data-binary`.
- Leaking the key: the key belongs on the client side only. Inside an instance, use the instance `env` for third-party credentials.
