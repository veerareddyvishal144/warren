# `warren run`, `warren shell` and `warren sync`

Three CLI commands turn an instance into the place where an agent's commands
run, with your project synced in and the results synced back.

## `warren run -- <command>`

Binds the current directory to an instance, pushes the directory into it,
runs the command there on a real terminal, pulls the files the command
changed back into the directory, and exits with the command's exit code.

```sh
cd my-project
warren run -- claude                  # Claude Code, interactive, inside a Linux sandbox
warren run --type mac -- claude       # the same inside a macOS VM (Apple silicon host)
warren run --type mac -- xcodebuild -scheme App -destination 'platform=iOS Simulator,name=iPhone 16' test
warren run --no-tty -- npm test       # no terminal: captured output, printed at the end
warren run --unbind --delete          # forget the binding and destroy the instance
```

- The binding lives in `.warren/instance.json` (instance id, remote root);
  add `.warren/` to `.gitignore`, the first run tells you when it is not.
- The first run creates the instance: template `warren-base` by default,
  `warren-macos` for `--type mac`, or `--template` (for example
  `warren-claude`, which ships Claude Code, Codex and OpenCode; the macOS base
  image built with `--with-agents` ships the same three). Later runs reuse
  the instance, waking or starting it when it is asleep or stopped.
- The directory maps to `<home>/work/<directory name>` inside the instance
  (`/home/agent/work/...` on Linux, `/Users/lume/work/...` on macOS);
  `--remote-root` changes it. The command runs with that directory as its
  working directory.
- `--new` creates a fresh instance even when the directory is bound;
  `--no-sync` skips both transfers.

## What is synced

Push sends every file under the current directory that `.gitignore` and
`.ignore` do not exclude, with `.git` included so `git status` and `git diff`
work inside, minus these directory names at any depth: `node_modules`, `target`, `dist`, `build`, `.next`, `.venv`, `__pycache__`, `.cache`, `.warren`. Add more
with `--exclude NAME`. The files travel as one gzip tarball through the files
API and are unpacked with `tar -xzm` so they carry the extraction time; a
marker file is then touched.

Pull lists the regular files under the remote root that are newer than the
marker (same excludes), archives them in the instance, fetches the archive and
unpacks it into the current directory, then moves the marker so nothing
written during the pull is missed next time. Deletions are not mirrored in
either direction. Generated trees such as `node_modules` stay in the instance.

`warren sync push <id>` and `warren sync pull <id>` run the two halves by
hand against any instance, with the same flags.

## `warren shell`

```sh
warren shell <id>                      # a login shell on a terminal
warren shell <id> -- 'tail -f app.log' # one command on a terminal
warren shell <id> --cwd work/my-project
```

The terminal is the gateway's `GET /v1/pty` WebSocket (see
[API.md](API.md)), reached through the edge with your API key, so it works
for every instance type and from any machine that reaches the daemon. The
local terminal is put in raw mode, window resizes are forwarded, and the
command's exit code becomes the CLI's.

## Limits

- One terminal per `warren shell` or `warren run`; several can run at once.
- The sync is directory-level and file-based; it is not a filesystem mount.
  Very large trees are slow to push the first time; exclude what you can.
- Deletions do not propagate. Remove files on both sides by hand.
- On macOS instances the user is `lume` and the home is `/Users/lume`.
