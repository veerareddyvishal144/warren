# Testing Warren

Three layers: unit tests inside every crate (fast, no Docker), the end-to-end
suite in `crates/warren-e2e` (a real `warrend` against Docker), and the vm
scenario (Firecracker, root and `/dev/kvm`). The last section is a manual
`curl` walkthrough of every feature in [API.md](API.md).

Always start with the environment from `scripts/env.sh`; it puts the toolchain,
caches and `TMPDIR` under `/data` (the host `/tmp` is `noexec`):

```bash
source scripts/env.sh
```

## Unit tests

Every crate tests itself with in-memory fakes of the traits it consumes
(`Store`, `Runtime`, ...). Nothing here needs Docker.

```bash
cargo test --workspace --exclude warren-e2e          # everything (also: make test)
cargo test -p warren-core                            # one crate
cargo test -p warren-e2e                             # the e2e harness's own unit tests only
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

When several people or agents build at once, use a private target directory
seeded from the shared one (see [DEVELOPMENT.md](DEVELOPMENT.md)):

```bash
export CARGO_TARGET_DIR=/data/warren/target-<crate>
[ -d "$CARGO_TARGET_DIR" ] || cp -a /data/warren/target "$CARGO_TARGET_DIR"
cargo test --locked -p <crate>
```

Runtime crates skip the tests that need Docker or `/dev/kvm` when those are
unavailable.

## End-to-end suite

`crates/warren-e2e/tests/e2e.rs` walks the whole contract against a real
daemon: signup and workspace, create/exec/edge auth, agent chat and streaming,
the files API, lifecycle (stop/start/restart/sleep/wake/auto-sleep), fork,
backup/restore, resize, update, the awake limit, crons and workspace templates
(registered images with the injected gateway, see below).
Every test is `#[ignore]`d so `cargo test --workspace` never touches Docker.

One command builds the binaries, builds the images and runs the suite:

```bash
scripts/e2e.sh                       # everything except vm_*
scripts/e2e.sh -- lifecycle          # one test (any substring filter)
scripts/e2e.sh --no-build --no-images -- crons   # reuse binaries and images
scripts/e2e.sh --keep                # keep each daemon's data dir and log
make e2e                             # the Makefile equivalent
```

By hand, the suite needs `target/release/warrend` (or `WARREN_BIN`), the static
gateway `target/x86_64-unknown-linux-musl/release/warren-gateway` (or
`WARREN_GATEWAY_BIN`; the daemon injects it into registered images) and the
images `warren/base:dev` and `warren/agent:dev`:

```bash
make build                     # warrend, warren, warren-mcp and the static musl gateway (once: make musl-setup)
make images                    # warren/base, warren/agent, warren/opencode, warren/claude carrying that gateway
cargo test -p warren-e2e -- --ignored --test-threads=1 --nocapture
```

What the harness does for each test:

- resolves the static gateway: `WARREN_GATEWAY_BIN`, else `<repo>/target/x86_64-unknown-linux-musl/release/warren-gateway`, else `<repo>/target-musl/x86_64-unknown-linux-musl/release/warren-gateway`, else the same path under `$CARGO_TARGET_DIR`; when none exists the daemon is not started and the error says how to build one;
- picks a free port and starts `warrend --dev --listen [::]:PORT --public-port PORT --data-dir <tempdir>/data --base-domain warren.localhost --max-awake-instances 3 --min-idle-seconds 5 --gateway-bin <gateway> --log info`, capturing its output in `<tempdir>/warrend.log`;
- waits for `GET http://api.warren.localhost:PORT/v1/health`;
- builds its HTTP clients with `warren_sdk::client_with_resolve`, pinning `api.warren.localhost` and every instance hostname to `127.0.0.1`, so the tests never depend on the OS resolving `*.localhost`;
- signs up a fresh workspace, runs the scenario with a 180 s wait for `running` and a 290 s budget per test, deletes the instances it created (also when the test failed or panicked), prints the daemon log tail on failure, and kills the daemon on drop (SIGTERM, then SIGKILL after 15 s).

Knobs: `WARREN_BIN` (daemon binary), `WARREN_GATEWAY_BIN` (the static gateway
passed as `--gateway-bin`), `WARREN_E2E_LOG` (daemon log filter),
`WARREN_E2E_IMAGE_TAG` (the daemon's `--image-tag`, to test images built under
another tag),
`WARREN_E2E_KEEP=1` (keep temp dirs), `WARREN_E2E_TMP` (where temp dirs go),
`WARREN_E2E_BASE_DOMAIN`.

### The templates scenario

`templates` proves that a registered image which ships neither the gateway nor
a long-running process becomes a working instance, because the daemon injects
the static gateway at `/warren/warren-gateway` and runs it as the entrypoint in
wrap mode (the image's own entrypoint and command run as a supervised child).
After the catalog and validation checks it asserts:

- `e2e-alpine`, registered from `alpine:3.20` with `mount_path: /home` and
  `default_port: 0`, reaches `running` although alpine's `/bin/sh` exits at
  once without a stdin;
- `exec cat /etc/alpine-release` contains `3.20`; `/home` is writable; the
  injected binary exists at `/warren/warren-gateway` (checked as root);
- `GET http://<id>.<base>/` with the key answers `502` "this template has no
  default port";
- `GET http://<id>-3737.<base>/v1/health` with the key answers `200` with
  `"healthy": true` and a `wrapped` object; the test polls (up to 60 s) until
  `wrapped.running` is `false` and accepts any `wrapped.exit_code` that is an
  integer or null (sh exits 0 when it has no stdin);
- the files API works through `http://<id>-3737.<base>/`: write
  `e2e/hello.txt`, read it back, list `e2e`, then `exec cat /home/e2e/hello.txt`
  sees it under the mount path;
- crons are `400 not_supported` on a non-agent template; deleting the template
  while the instance exists is `409 conflict`; a second revision publishes and
  the instance keeps revision 1;
- `e2e-node`, registered from `node:24-bookworm-slim` (its `CMD` is `node`,
  which also exits at once) with `default_port: 3737` and `mount_path:
  /home/node`, reaches `running`; `GET <instance url>/v1/health` with the key
  answers `200` with `wrapped` (the bare URL reaches the injected gateway);
  `exec node --version` is `v24`; a file written through the bare URL runs with
  `node /home/node/hello.js`; the instance and the template are deleted;
- the alpine instance is deleted, then the template, which is then `404`.

The daemon pulls `alpine:3.20` and `node:24-bookworm-slim` on registration if
they are not local; `scripts/e2e.sh` pulls them beforehand.

If a test is killed before cleanup, leftover containers are visible with
`docker ps -a` (the docker runtime names and labels them with the instance id);
remove them with `docker rm -f <container>`. Temp directories live under
`$TMPDIR/warren-e2e-*`.

The awake limit the suite exercises is the daemon's `--max-awake-instances`
(`WARREN_MAX_AWAKE_INSTANCES`); the harness starts every daemon with 3, and
`GET /v1/workspace` reports it as `awake_limit` (`null` when unset, meaning
unlimited) next to `awake_count`.

## Troubleshooting the e2e suite

- `static warren-gateway binary not found` when a test starts: the harness
  insists on the gateway the daemon injects into registered images. Build it
  with `make gateway` (once: `make musl-setup` for `musl-tools` and the
  `x86_64-unknown-linux-musl` target) or point `WARREN_GATEWAY_BIN` at one.
- `e2e-alpine` or `e2e-node` never reaches `running`, or `wrapped` is missing
  from the health answer: check the daemon log for `gateway injection enabled
  for registered images`; `no warren-gateway binary found` means the daemon was
  started without `--gateway-bin`. Make sure the binary is static and runs in
  Alpine: `docker run --rm -v $PWD/target/x86_64-unknown-linux-musl/release/warren-gateway:/warren/warren-gateway:ro alpine:3.20 /warren/warren-gateway --version`
  (a glibc build fails there with "not found").
- `instance ... did not listen on port 3737 within 90 s` in the log tail: the
  image's gateway never came up. Check that `warren/base:<tag>` carries the real
  binary (`docker run --rm --entrypoint /usr/local/bin/warren-gateway warren/base:dev --version`);
  a stand-in script makes every scenario fail this way. Rebuild with
  `make gateway && WARREN_SKIP_GATEWAY_BUILD=1 scripts/build-images.sh base agent`.
  To test images built under another tag without touching `:dev`, build with
  `WARREN_IMAGE_TAG=<tag>` and run the suite with `WARREN_E2E_IMAGE_TAG=<tag>`.
- `remove storage: Permission denied` when an instance is deleted, or
  `could not remove /data/tmp/warren-e2e-...`: files written through the files
  API are owned by root on the host (the gateway runs as root in the
  container), so a daemon running as the dev account cannot delete them. The
  harness removes such leftovers through a throwaway `alpine:3.20` container
  and says so.
- The daemon shuts down with SIGTERM when a test ends; if a test process is
  killed outright, stop the orphaned `warrend` (`pkill -f 'warrend --dev'`) and
  remove its containers (`docker ps -a`).

## The vm test

`vm_create_exec` creates a `type: vm` instance from `warren-base`, execs in it,
reaches its URL and deletes it. It runs only with `WARREN_E2E_VM=1`, which also
makes every daemon the suite starts run with `--firecracker --kernel <kernel>`
(and `--firecracker-bin <bin>` when the probe binary exists). It needs root,
`/dev/kvm`, a Firecracker binary and a kernel:

```bash
scripts/e2e.sh --vm                      # whole suite plus vm_*, under sudo
scripts/e2e.sh --vm -- vm_               # just the vm scenario
WARREN_E2E_VM=1 WARREN_E2E_KERNEL=/data/sandbox-probe/vmlinux-6.1.155 \
WARREN_E2E_FIRECRACKER_BIN=/data/sandbox-probe/firecracker \
  sudo -E env PATH=$PATH CARGO_HOME=$CARGO_HOME RUSTUP_HOME=$RUSTUP_HOME TMPDIR=$TMPDIR \
  cargo test -p warren-e2e -- --ignored --test-threads=1 --nocapture vm_
```

`make e2e-vm` runs the same `sudo` command; export `WARREN_E2E_VM=1` first,
otherwise the test prints that it was skipped. Without KVM the daemon logs
"firecracker runtime not ready; vm instances disabled" and the test fails with
that message in the log tail.

## The mac test (Apple silicon only)

`mac` instances cannot be tested on a Linux host. `crates/warren-runtime-mac`
unit-tests the runtime against a mock of Lume's API and a fake gateway
(`cargo test -p warren-runtime-mac`), and `scripts/mac/e2e.sh` exercises the
real thing on a Mac: it builds `warrend` and the native gateway, starts the
daemon with `--runtime none --mac-base-vm <vm>`, then creates a `mac`
instance, execs in it, suspends and resumes it (a background process must
survive), forks it, checks that one guest cannot reach the other's gateway,
provokes `409 host_capacity_reached` with a third instance and deletes
everything. `scripts/mac/README.md` lists the prerequisites (Lume 0.7.2+, a
base VM prepared with `scripts/mac/build-base.sh`, `jq`).

## Dashboard smoke test (real browser)

`scripts/dashboard-smoke.mjs` drives the embedded dashboard in headless Chrome against a dev daemon it starts itself: signup through the UI, create an instance from the form, wait for `running`, run a command in the exec console, delete the instance. It fails on any browser console error, page error or failed request.

```bash
make build                          # release binaries and the static gateway
npm i --no-save playwright-core     # one-time, in the repo root
CHROME=/usr/bin/google-chrome node scripts/dashboard-smoke.mjs
```

Environment: `PORT` (default 7471), `CHROME`, `WARREN_BIN`, `WARREN_GATEWAY_BIN`, `WARREN_SMOKE_DATA` (default `data/dashboard-smoke`). On this host it passes in about 25 seconds.

## Manual walkthrough with curl

Start a daemon in dev mode (release build, data under `./data`, dashboard URL
printed):

```bash
scripts/dev-up.sh            # or: make dev-up
```

On this host `*.localhost` resolves to `::1` and the daemon binds dual-stack,
so the hostnames below just work. Elsewhere add
`--resolve api.warren.localhost:7337:127.0.0.1` (and the same for each instance
host) to every `curl`. `jq` is used to pull ids out of answers.

```bash
export API=http://api.warren.localhost:7337/v1
export BASE=warren.localhost:7337
```

### Health, version, signup

```bash
curl -s $API/health | jq .                      # {"healthy":true,"version":...,"runtimes":["docker"],...}
curl -s $API/version | jq .
curl -s -X POST $API/signup -H 'content-type: application/json' -d '{"name":"demo"}' | tee /tmp/signup.json | jq .
export KEY=$(jq -r .api_key /tmp/signup.json)   # sk_live_..., shown once (201: workspace, api_key, key_id)
```

### Workspace and keys

```bash
curl -s $API/workspace -H "authorization: bearer $KEY" | jq .          # awake_limit (number, or null when --max-awake-instances is unset), awake_count 0
curl -s $API/api-keys -H "authorization: bearer $KEY" | jq .
curl -s -X POST $API/api-keys -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"name":"ci"}' | tee /tmp/key.json | jq .
curl -s -X DELETE $API/api-keys/$(jq -r .key.id /tmp/key.json) -H "authorization: bearer $KEY" | jq .
curl -s $API/workspace -H "x-warren-key: $KEY" | jq .awake_limit       # X-Warren-Key works too
```

### Templates

```bash
curl -s $API/templates -H "authorization: bearer $KEY" | jq '.data[].name'    # warren-base, warren-agent, warren-opencode, warren-claude
curl -s $API/templates/warren-agent -H "authorization: bearer $KEY" | jq .
curl -s -X POST $API/templates -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"name":"my-alpine","image_ref":"alpine:3.20","mount_path":"/home","default_port":0}' | jq .   # 201, revision 1
curl -s $API/templates/my-alpine@1 -H "authorization: bearer $KEY" | jq .revision
```

A registered image gets the gateway injected at `/warren/warren-gateway` and its
own entrypoint wrapped, so even `alpine:3.20`, whose `/bin/sh` exits at once,
becomes a running instance with exec, files and health
([templates.md](templates.md)):

```bash
export ALPINE=$(curl -s -X POST $API/instances -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"template":"my-alpine"}' | jq -r .id)
until [ "$(curl -s $API/instances/$ALPINE -H "authorization: bearer $KEY" | jq -r .status)" = running ]; do sleep 2; done
curl -s -X POST $API/instances/$ALPINE/exec -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"command":"cat /etc/alpine-release"}' | jq -r .stdout                       # 3.20.x
curl -s -i http://$ALPINE.$BASE/ -H "x-warren-key: $KEY" | head -1                    # 502: default_port 0, the bare URL serves nothing
curl -s http://$ALPINE-3737.$BASE/v1/health -H "x-warren-key: $KEY" | jq .            # {"healthy":true,...,"wrapped":{"running":false,"exit_code":0}}
curl -s -X PUT "http://$ALPINE-3737.$BASE/v1/files/content?path=hello.txt" -H "x-warren-key: $KEY" --data-binary 'hi' | jq .
curl -s -X POST $API/instances/$ALPINE/exec -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"command":"cat /home/hello.txt"}' | jq -r .stdout                             # hi
curl -s -X DELETE $API/templates/my-alpine -H "authorization: bearer $KEY" | jq .error.code   # conflict while the instance exists
curl -s -X DELETE $API/instances/$ALPINE -H "authorization: bearer $KEY" | jq .
```

### Instances: create, inspect, patch

```bash
curl -s -X POST $API/instances -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"template":"warren-agent","name":"demo","env":{"WARREN_AGENT_DRIVER":"echo"}}' | tee /tmp/inst.json | jq .
export ID=$(jq -r .id /tmp/inst.json)                                  # status provisioning, url http://$ID.warren.localhost:7337
until [ "$(curl -s $API/instances/$ID -H "authorization: bearer $KEY" | jq -r .status)" = running ]; do sleep 2; done
curl -s $API/instances -H "authorization: bearer $KEY" | jq '.data[] | {id,status,template}'
curl -s -X PATCH $API/instances/$ID -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"name":"renamed","metadata":{"team":"demo"},"auto_sleep":true,"idle_timeout_seconds":900}' | jq '{name,metadata,auto_sleep}'
```

### Exec, logs, metrics

```bash
curl -s -X POST $API/instances/$ID/exec -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"command":"uname -a && id -u"}' | jq .                          # exit_code, stdout, stderr, duration_ms
curl -s -X POST $API/instances/$ID/exec -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"command":"id -u","user":"root"}' | jq -r .stdout               # 0
curl -s "$API/instances/$ID/logs?tail=50" -H "authorization: bearer $KEY" | jq -r .logs
curl -s "$API/instances/$ID/metrics?hours=1" -H "authorization: bearer $KEY" | jq .   # one point per minute while running
```

### Instance URLs through the edge

```bash
curl -s http://$ID.$BASE/v1/health -H "x-warren-key: $KEY" | jq .     # the gateway inside the instance
curl -s -i http://$ID.$BASE/v1/health | head -1                        # no key -> 401 invalid_api_key
curl -s -i http://$ID-7681.$BASE/ -H "x-warren-key: $KEY" | head -1    # preview port: ttyd, the browser terminal
curl -s -i http://$ID-5999.$BASE/ -H "x-warren-key: $KEY" | head -1    # nothing listens -> 502
curl -s -X OPTIONS -i http://$ID.$BASE/v1/health -H 'origin: http://example.test' | grep -i access-control
```

Signed URLs: a token in the query becomes the `warren_session` cookie.

```bash
URL=$(curl -s "$API/instances/$ID/signed-url?port=7681&ttl=600&path=/" -H "authorization: bearer $KEY" | jq -r .url)
curl -s -i "$URL" | grep -i -E '^(HTTP|location|set-cookie)'           # 302, Location without the token, Set-Cookie warren_session=...
curl -s -i -c /tmp/jar -b /tmp/jar -L "$URL" | head -1                 # follow with a cookie jar -> 200 (open $URL in a browser for the terminal)
```

### Agent API (agent templates)

```bash
curl -s -X POST http://$ID.$BASE/v1/responses -H "x-warren-key: $KEY" -H 'content-type: application/json' \
  -d '{"input":"hi"}' | tee /tmp/resp.json | jq .                       # echo driver: output_text "echo: hi"
export SID=$(jq -r .session_id /tmp/resp.json)
curl -s -N -X POST http://$ID.$BASE/v1/responses -H "x-warren-key: $KEY" -H 'content-type: application/json' \
  -d "{\"input\":\"stream me\",\"session_id\":\"$SID\",\"stream\":true}"   # event: response.created / response.output_text.delta / response.completed
curl -s -X POST http://$ID.$BASE/v1/responses -H "x-warren-key: $KEY" -H 'content-type: application/json' \
  -d '{"input":"!echo hello"}' | jq -r .output_text                    # echo driver runs the shell command
curl -s http://$ID.$BASE/v1/responses/$(jq -r .id /tmp/resp.json) -H "x-warren-key: $KEY" | jq .
curl -s http://$ID.$BASE/v1/sessions -H "x-warren-key: $KEY" | jq .    # newest first, message_count
curl -s http://$ID.$BASE/v1/sessions/$SID -H "x-warren-key: $KEY" | jq '.messages[] | {role,content}'
curl -s -X PATCH http://$ID.$BASE/v1/sessions/$SID -H "x-warren-key: $KEY" -H 'content-type: application/json' -d '{"name":"first"}'
curl -s http://$ID.$BASE/v1/models -H "x-warren-key: $KEY" | jq .
curl -s -X DELETE http://$ID.$BASE/v1/sessions/$SID -H "x-warren-key: $KEY"
```

### Files API (every template)

```bash
curl -s -X PUT "http://$ID.$BASE/v1/files/content?path=notes/a.txt" -H "x-warren-key: $KEY" --data-binary 'hello files' | jq .  # {"path","size":11}
curl -s "http://$ID.$BASE/v1/files/content?path=notes/a.txt" -H "x-warren-key: $KEY"
curl -s "http://$ID.$BASE/v1/files?path=notes" -H "x-warren-key: $KEY" | jq .
curl -s -X POST http://$ID.$BASE/v1/files/move -H "x-warren-key: $KEY" -H 'content-type: application/json' \
  -d '{"from":"notes/a.txt","to":"notes/b.txt"}' | jq .
curl -s "http://$ID.$BASE/v1/files/download?path=notes" -H "x-warren-key: $KEY" -o notes.tar.gz && tar tzf notes.tar.gz
curl -s -X DELETE "http://$ID.$BASE/v1/files?path=notes/b.txt" -H "x-warren-key: $KEY" | jq .
curl -s -i "http://$ID.$BASE/v1/files/content?path=../../etc/passwd" -H "x-warren-key: $KEY" | head -1   # 403 forbidden
```

### Lifecycle: stop, start, sleep, wake, restart

```bash
curl -s -X POST $API/instances/$ID/stop -H "authorization: bearer $KEY" | jq .      # stopping -> stopped; URL answers 409
curl -s -X POST $API/instances/$ID/start -H "authorization: bearer $KEY" | jq .     # starting -> running (enforces the awake limit)
curl -s -X POST $API/instances/$ID/sleep -H "authorization: bearer $KEY" | jq .     # sleeping now; does not count as awake
curl -s http://$ID.$BASE/v1/health -H "x-warren-key: $KEY" | jq .                   # held while the instance wakes, then 200
curl -s $API/instances/$ID -H "authorization: bearer $KEY" | jq .status             # running
curl -s -X POST $API/instances/$ID/restart -H "authorization: bearer $KEY" | jq .
```

Auto-sleep: with `--dev` the minimum idle timeout is 5 s.

```bash
curl -s -X PATCH $API/instances/$ID -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"auto_sleep":true,"idle_timeout_seconds":5}' | jq .auto_sleep
sleep 40; curl -s $API/instances/$ID -H "authorization: bearer $KEY" | jq .status   # sleeping
curl -s -X POST $API/instances/$ID/exec -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"command":"echo awake"}' | jq -r .stdout                                      # exec wakes it
```

### Resize, update, fork

```bash
curl -s -X POST $API/instances/$ID/resize -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"cpu":4,"memory":8}' | jq .                                     # valid shapes: 2/4, 4/8, 4/16, 4/32; disk only grows
curl -s -X POST $API/instances/$ID/update -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"template":"warren-agent"}' | jq .                              # updating -> running, /home/agent kept
curl -s -X POST $API/instances/$ID/fork -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"name":"copy"}' | tee /tmp/fork.json | jq '{id,parent_id,status}'   # 201; a running source is slept, copied and woken
export FORK=$(jq -r .id /tmp/fork.json)
```

### Backups and restore

```bash
curl -s -X POST $API/instances/$ID/backups -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"note":"before the change"}' | tee /tmp/backup.json | jq .     # 201, taken without stopping
curl -s $API/instances/$ID/backups -H "authorization: bearer $KEY" | jq .
curl -s -X POST $API/instances/$ID/restore -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d "{\"backup_id\":\"$(jq -r .id /tmp/backup.json)\"}" | jq .status  # stopped, restored, restarted if it was running
```

### Crons (agent templates only)

```bash
curl -s -X POST $API/instances/$ID/crons -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"prompt":"summarise the day","schedule":"* * * * *","name":"every-minute"}' | tee /tmp/cron.json | jq .   # 201, next_run_at set
export CID=$(jq -r .id /tmp/cron.json)
curl -s $API/instances/$ID/crons -H "authorization: bearer $KEY" | jq .
sleep 90; curl -s $API/instances/$ID/crons/$CID -H "authorization: bearer $KEY" | jq '{last_run_at,last_status,next_run_at}'
curl -s http://$ID.$BASE/v1/sessions -H "x-warren-key: $KEY" | jq '.data | length'   # each firing adds a session
curl -s -X PATCH $API/instances/$ID/crons/$CID -H "authorization: bearer $KEY" -H 'content-type: application/json' -d '{"enabled":false}' | jq .enabled
curl -s -X DELETE $API/instances/$ID/crons/$CID -H "authorization: bearer $KEY" | jq .
```

### Limits and errors

```bash
for i in 1 2 3 4; do curl -s -X POST $API/instances -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"template":"warren-base"}' | jq -c '{id,status,error}'; done   # with --max-awake-instances 3 the 4th awake instance: 409 instance_limit_reached
curl -s -X POST $API/instances -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"template":"warren-base","type":"performance"}' | jq .error.code   # not_supported
curl -s -X POST $API/instances -H "authorization: bearer $KEY" -H 'content-type: application/json' \
  -d '{"bogus":1}' | jq .error.code                                     # invalid_request: unknown fields are rejected
```

### Delete

```bash
curl -s -X DELETE $API/instances/$FORK -H "authorization: bearer $KEY" | jq .    # {"id","status":"deleting"}
curl -s -X DELETE $API/instances/$ID -H "authorization: bearer $KEY" | jq .
curl -s $API/instances -H "authorization: bearer $KEY" | jq '.data | length'
curl -s -X DELETE $API/templates/my-alpine -H "authorization: bearer $KEY" | jq .   # once no instance uses it
```

The dashboard at `http://warren.localhost:7337/` drives the same API from a
browser: paste the key, create instances, open the terminal through a signed
URL, chat and exec.
