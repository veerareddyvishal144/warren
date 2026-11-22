---
name: warren
description: Use Warren sandboxes (persistent Linux or macOS computers with exec, files, preview URLs, sleep and wake, fork) through the warren MCP tools or the warren CLI whenever a task needs an isolated machine, a long-lived environment that survives the session, a macOS or Xcode box, a server someone can open in a browser, or commands that should not run on the user's own computer.
---

# Warren sandboxes

Warren gives you persistent computers ("instances") you drive over an API. Each one has a shell, a home directory that survives sleep and restarts, a URL per port, and optional agents inside. Prefer the MCP tools when they are available (server `warren`); the `warren` CLI does the same from a terminal.

## When to use one

- Build, test or run anything untrusted, heavy or long-running: do it in an instance, not on the user's machine.
- Keep a working environment across sessions: files under the instance home persist; processes do not survive sleep, stop or restart.
- Need macOS (Xcode, the Simulator, Safari): create a `mac` instance on an Apple silicon host.
- Show a running server to a human: start it in the instance and share a signed preview URL.

## Create

MCP: `create_instance` with `template` and optionally `type`, `name`, `cpu`, `memory`, `disk`, `auto_sleep`, `idle_timeout_seconds`, `env`. It waits up to 120 s for `running`; otherwise poll `get_instance` until `status` is `running` before using it.

| Template | Type | What it is |
|---|---|---|
| `warren-base` | `default` (gVisor container) or `vm` (Firecracker) | Debian 12, Node, Python 3, git, build tools; user `agent`, home `/home/agent`, passwordless sudo |
| `warren-claude` | `default` or `vm` | `warren-base` plus the `claude`, `codex` and `opencode` CLIs; put provider keys in `env` |
| `warren-macos` | `mac` | A macOS VM on an Apple silicon Mac; user `lume`, home `/Users/lume`; the template alone implies the type |

Shapes are `cpu/memory` in vCPU/GB: 2/4, 4/8, 4/16, 4/32 (`mac` needs at least 4 GB). Use `auto_sleep: true` unless a server must stay up. Credentials for tools inside the instance go in `env` at create time; never put the Warren API key in an instance.

CLI: `warren instances create --template warren-base --wait`, `warren instances create --template warren-macos --wait` (or `--type mac`), `--env KEY=value`, `--auto-sleep`.

## Work inside

- `exec` (`id`, `command`, `user?`, `timeout_seconds?`) runs a shell command and returns `exit_code`, `stdout`, `stderr`. A non-zero exit code is data, not an error. Output is capped at 1 MiB per stream; write big results to files. Background servers: `nohup cmd >log 2>&1 &`.
- Files, paths relative to the home: `read_file`, `write_file`, `list_files`. The CLI: `warren files ls|cat|put|rm|download <id> ...`.
- `chat` (`id`, `message`, `session_id?`) talks to the agent inside `warren-agent`/`warren-opencode` templates; pass `session_id` back to continue.
- Preview URLs: anything listening on port P is `http://<id>-P.<base>/` with the workspace key, or `signed_url` (`id`, `port?`, `ttl?`) for a link a browser or a human can open without the key. Port 7681 is the browser terminal in the Linux templates.
- CLI equivalents: `warren instances exec <id> -- <command>`, `warren shell <id>` (interactive terminal; `warren shell <id> -- cmd` for one command on a TTY), `warren instances url <id> --port 8000`, `warren instances open <id> --port 7681`, `warren chat <id> "..."`.
- `warren run -- <command>` creates (or reuses) an instance bound to the current directory, pushes the directory, runs the command on a terminal inside, and pulls changed files back: `warren run -- claude`, `warren run --type mac -- xcodebuild -scheme App`. `warren sync push|pull` moves files by hand; `warren run --unbind [--delete]` forgets the binding.

## Lifecycle

- `sleep_instance`: processes stop, the disk stays; any request to the instance wakes it (on `mac`, sleep keeps memory and wake resumes the processes). `stop_instance` never wakes on request; `start_instance` starts a stopped or sleeping one. CLI: `warren instances sleep|stop|start|restart <id>`.
- `delete_instance` removes the instance and its disk; delete what you no longer need. CLI: `warren instances delete <id>`.
- Fork copies disk, template and env into a new instance: `warren instances fork <id> --name copy` (no MCP tool yet).
- `list_instances`, `get_instance`, `list_templates` to look around; `warren instances list`, `warren templates list`.

## The macOS screen

- `mac` instances have a display. `screenshot` (`id`) returns a PNG image to look at: take one before acting to find coordinates and after acting to confirm. `click` (`id`, `x`, `y`, `button?`, `count?`), `type_text` (`id`, `text`; `\n` is Return), `press_key` (`id`, `key`: `enter`, `escape`, `cmd+space`, `cmd+q`, `ctrl+c`, `f5`, arrows) and `scroll` (`id`, `x`, `y`, `dx?`, `dy?`) drive it. Coordinates are pixels of the screenshot, origin top left.
- Prefer `exec` for anything a shell can do (`open -a Safari https://...`, `xcodebuild`, `osascript`); use the screen for dialogs, the Simulator, sign-in flows and to verify what the user would see. Linux instances have no screen (`not_supported`).
- CLI: `warren screenshot <id> -o shot.png`, `warren input <id> click X Y`, `warren input <id> type "text"`, `warren input <id> key cmd+space`.

## Limits to respect

- A Mac runs at most **two macOS guests** at once: a third `mac` create or wake answers `409 host_capacity_reached`. Sleep or delete a `mac` instance you are done with before creating another.
- The operator may cap awake instances per workspace (`409 instance_limit_reached`): sleep or delete one, then retry. Sleeping and stopped instances do not count.
- `409 conflict` on a stopped instance: start it first. `503 try_again`: wait a few seconds and retry.
- Wait for `running` after create, start and update. Packages installed outside the home are lost on sleep, stop and restart; reinstall at the start of a task or bake them into a template.

## Setup (for the human)

`warren up` starts the daemon on this machine, `warren connect claude` (or `codex`, `cursor`, `windsurf`, `caret`, `opencode`, `gemini`) registers the MCP server, `warren mac init` builds the macOS base VM on a Mac, `warren doctor` checks everything. Docs: `docs/quickstart.md`, `docs/for-agents.md`, `mcp/README.md`.
