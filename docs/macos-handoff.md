# Handoff: continuing the macOS work from a Claude Code session on the Mac

Written 2026-11-19 for a Claude Code session running directly on the owner's
MacBook Pro (Apple silicon), so the Mac-side testing no longer has to be
relayed through a Linux session. Read this, then `docs/macos.md`, then
`scripts/mac/README.md`. Everything below was true at commit `c4d9b28`.

## The project in one paragraph

Warren is an open-source, self-hosted agent sandbox platform written in Rust
(workspace at the repo root; `docs/API.md` is the wire contract,
`docs/DESIGN.md` the architecture, `docs/DEVELOPMENT.md` the rules). One
daemon, `warrend`, serves the Sandbox API, the edge proxy, lifecycle and
cron; `warren-gateway` runs inside every instance. Instance types: `default`
(gVisor container), `vm` (Firecracker), and the new `mac` (a macOS VM through
Lume on an Apple silicon host). There is deliberately no billing or money
anywhere; do not add any. Commits end with
`Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>`; push to
`origin main` (github.com/veerareddyvishal144/warren, private).

## All phases and their status

The plan and its design live in `docs/macos.md` ("Phases" and "Acceptance
for Phase 1"); this is the status as of this handoff.

| Phase | What | Status |
|---|---|---|
| 0 | Feasibility on this Mac: VM from IPSW, clone, two guests at once, boot-to-SSH 11 s, memory ~415 MB after boot, gateway runs inside a macOS VM | done |
| 1a | Runtime and daemon on Linux: `warren-runtime-mac`, `warren-gateway-client`, `InstanceType::Mac`, `host_capacity_reached`, `Runtime::sleep`, `--runtime none` and the Lume flags, `GET /v1/stats`, Open Directory user names, mock-API tests, CI macOS job | done, pushed |
| 1b | Base image and end-to-end on the Mac: `build-base.sh` (done, image built twice, second time with the clean shutdown), `e2e.sh` running through | in progress: passes health, signup, create, edge health, exec; fails at the marker process; sleep/wake/fork/isolation/capacity/delete not yet exercised |
| 1c | Surfaces and docs: dashboard type picker, CLI/MCP/SDK, `docs/*`, operator guide `docs/macos.md`, CHANGELOG | done, pushed; final numbers and status line still to update once 1b passes |
| 2 | Candidates, not started: screen access (noVNC through the edge), Linux guests through Lume on Mac hosts, one `lume serve` per instance, a `mac-xcode` template, Mac minis as nodes behind one daemon | not started; do not begin before 1b passes twice |

Definition of done for Phase 1 (from `docs/macos.md`): `e2e.sh` green twice
in a row on this Mac, meaning create to healthy gateway under 60 s, sleep
suspends and frees a slot, wake resumes with processes intact in under
15 s, the third create answers `409 host_capacity_reached`, delete removes
the VM and the share folder, a daemon restart adopts running VMs, and one
guest cannot reach another's gateway port. Then update `docs/macos.md`
(status line, "Running it" numbers), `CHANGELOG.md`, and delete this file.

## Where the macOS work stands

Implemented and verified on Linux (unit tests, clippy for Linux and
`aarch64-apple-darwin`, the Docker end-to-end suite): the `mac` type end to
end in code. On the Mac, `scripts/mac/e2e.sh` has passed these steps:

- health reports runtime `lume`; signup;
- create a `mac` instance: clone of the base VM, boot, gateway healthy, in
  21 s;
- gateway health through the edge, with `home_owner.name` = `lume`;
- `exec` of `sw_vers && whoami`.

It fails at the next step, "start marker process": the test runs
`nohup sleep 100000 </dev/null >/dev/null 2>&1 & echo started` through
`POST /v1/instances/{id}/exec` and then `pgrep -f '[s]leep 100000'`, and the
process is not found. The same two exec calls against the gateway on Linux
leave the process alive and `pgrep` finds it (the gateway kills a command's
process group only on timeout; see `crates/warren-gateway/src/exec.rs`,
`run_capped`). So this is macOS-specific. Candidates: launchd treating the
orphaned process differently because the gateway is itself a launchd job
(`/Library/LaunchDaemons/dev.warren.gateway.plist`, `UserName lume`), the
login-shell `sh -lc` on macOS, or `pgrep -f` behaving differently there.
`scripts/mac/debug-guest.sh` boots a throwaway clone with a share attached
and, among other sections, tries four ways of leaving a process behind
(plain `nohup &`, perl `setsid`, nested `sh -c`, `launchctl submit`) and
prints `ps` and `pgrep` results. Run it first; its output decides whether
the fix belongs in the gateway (spawn children in their own session, or
keep a `launchctl`-friendly path) or only in the test's marker command.

After that, the remaining e2e steps have never run on a Mac: sleep
(suspend, `savedState: true` in `lume get`), wake (the marker must survive
with the same pid), fork, guest-to-guest isolation through `pf`, the third
create answering `409 host_capacity_reached`, and delete. Expect small fixes
in `scripts/mac/e2e.sh` or `crates/warren-runtime-mac`.

## Facts about this Mac

- Lume 0.7.2 at `~/.local/share/lume/lume.app/Contents/MacOS/lume` (on PATH
  as `lume`). `lume serve` is started by `warrend` on `127.0.0.1:7777` and
  is never stopped by it; it may already be running.
- VMs (`lume ls`): `sandbox` (the hand-built macOS 26.6.2 VM, SSH key
  `~/.ssh/id_ed25519` installed, user `lume`, password `lume`), `base` (an
  older clean clone without the key), `warren-macos-26` (the Warren base
  image built by `scripts/mac/build-base.sh --source sandbox`, stopped).
  Instance VMs are named `warren-<10 char id>`; `dbg-*` are probe clones.
- Port 7337 on this Mac is taken by an SSH tunnel; use `--port 7347`.
- `*.localhost` does not resolve for curl on macOS; the scripts send `Host:`
  headers to `127.0.0.1:<port>` instead. Browsers resolve it fine:
  `http://warren.localhost:7347` is the dashboard when the daemon runs.
- The daemon's data directory for tests is `./data-mac-e2e` (log at
  `data-mac-e2e/warrend.log`); `.gitignore` covers it.
- cargo lives under `~/.cargo/bin`; scripts add it to PATH themselves.

## Lessons already learned here (do not rediscover)

- `lume run` holds the VM in its process; `--detach` backgrounds it. VMs
  started through the API live in the `lume serve` process; `suspend` works
  only for those.
- `lume get -f json` prints a one-element array. `GET /lume/host/status` and
  `GET /lume/config` are snake_case; VM objects are camelCase. The runtime
  accepts both.
- Powering a VM off right after writing files loses them: the first base
  image came up without its LaunchDaemons. `build-base.sh` now syncs and
  shuts macOS down from inside; the runtime gives a guest at least 45 s to
  stop.
- pf `pass` rules must be `flags any no state`, or enabling pf cuts the
  connections that already exist.
- The shared folder is at `/Volumes/My Shared Files/share` in the guest;
  the launcher copies the gateway to `/Users/lume/.warren/` and sources
  `warren.env` (shell-safe `export KEY='value'` lines).

## How to work

```sh
cd ~/warren && git pull
cargo build --release -p warrend -p warren-gateway        # native arm64 build
bash scripts/mac/debug-guest.sh                           # guest diagnostics
bash scripts/mac/e2e.sh --port 7347 --clean-leftovers     # the whole scenario
bash scripts/mac/build-base.sh --source sandbox --target warren-macos-26 --force   # only if the base image must change
```

Linux-only checks you cannot run here: the Docker e2e (`make e2e`), the
Firecracker test, and the full unit suite's Docker-dependent tests;
`cargo test -p warren-runtime-mac -p warren-gateway -p warren-core
-p warren-lifecycle` runs fine on the Mac. Keep every crate compiling for
both Linux and macOS; Linux-only code goes behind `#[cfg(target_os =
"linux")]` with a stub (see `crates/warren-gateway/src/init_stub.rs`).

When the e2e passes twice in a row, update `docs/macos.md` (status line and
the "Running it" numbers), `CHANGELOG.md`, and remove this file.
