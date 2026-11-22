# Benchmarks

Measured numbers for Warren instances, and the method behind each one. Nothing on this page is a target or a projection: every figure was observed on the machine named next to it, and the figures that have not been measured yet say so. When you measure something new, add it here with its method; do not round a guess into a number.

## macOS instances (type `mac`)

**Setup.** One MacBook Pro, Apple silicon, 16 GiB unified memory. Measured on 2026-11-19. Guests are macOS 26.6.2, built from Apple's `UniversalMac_26.6.2_25G83_Restore.ipsw` and prepared with `scripts/mac/build-base.sh`; Lume 0.7.2; `warrend` built natively for `aarch64-apple-darwin` and started by `scripts/mac/e2e.sh` with `--runtime none --mac-base-vm warren-macos-26 --dev`. Guest shape 4 vCPU / 4 GB RAM / 50 GB disk (the base image's size). The host ran nothing else of note during the runs.

**Method.** `scripts/mac/e2e.sh` times each step with the shell's `SECONDS` counter around the API call and the status poll that follows it, polling `GET /v1/instances/{id}` every 2 s. Every figure therefore includes the API round trip, the lifecycle's work, the gateway's first health answer and up to 2 s of polling granularity; they are what a client experiences, not what the hypervisor reports. Memory figures come from manual probes on the same day: inside the guest from the gateway's `GET /v1/stats` (which reads `vm_stat` and `sysctl`), on the host from `ps -eo rss` over the Lume and Virtualization helper processes, the way `scripts/mac/boot-test.sh` prints them. Ranges are the minimum and maximum over the day's runs (four end-to-end runs plus manual repeats); there are too few samples for percentiles, so none are given.

### Time

| Operation | Measured | What the time contains |
|---|---|---|
| Create to `running` with a healthy gateway | 19 to 23 s | The APFS clone is instant; the rest is the macOS boot, the DHCP lease and the gateway's first `GET /v1/health`. |
| Sleep (suspend, memory kept) | 3 to 4 s | Lume writes the guest's memory next to its disk; the VM leaves the running set and frees one of the two slots. |
| Wake (resume from the saved state) | 5 to 11 s | Processes are intact: the e2e marker process is alive afterwards with the same pid. |
| `warrend` restart with a running instance | 6 to 7 s | The daemon stops and starts; the VM keeps running inside `lume serve` and the reconciler adopts it. Nothing changes inside the guest. |
| Fork of a running instance | 30 to 37 s | The source is suspended, cloned and resumed; the copy boots to a healthy gateway. |
| Delete two instances | 18 to 19 s | Lume's graceful stop (force after 10 s), its wait for locks, and removal of both VM directories and share folders. |
| Third create on a full host | immediate | `409 host_capacity_reached` before anything is cloned. |
| Cold boot of a stock VM to SSH (Phase 0) | 11 s | `scripts/mac/boot-test.sh` on the same Mac, before the gateway existed; the gap to 19 to 23 s above is the gateway's start and the client-side polling. |

`docs/macos.md` records an earlier set of four runs from the same day (create 21 to 23 s, delete about 20 s); the ranges above include the later manual repeats.

### Memory

| What | Measured | Where it was read |
|---|---|---|
| Idle macOS guest, as the guest sees it | 2.0 GB used of the 4 GB cap | Inside the guest: `GET /v1/stats` (`vm_stat`) |
| Cost of one idle guest to the host | about 0.3 to 0.5 GiB resident | On the host: RSS of the VM's Virtualization helper process |
| VM helper right after a cold boot (Phase 0) | 415 MB | On the host, `scripts/mac/boot-test.sh` |

The guest-side and host-side figures differ by a factor of four or more. We have not confirmed the mechanism; the likely explanation is that the host backs only the guest pages that have been touched and compresses or reclaims the rest, so the host figure is the one that matters for sizing an idle fleet while the guest figure is the one that matters once an agent starts Xcode. Treat both as idle numbers and re-measure under load before relying on either. Apple's minimum for a macOS guest is 4 GB and Virtualization.framework runs at most two macOS guests per host, so a 16 GiB Mac runs two guests with room for the host; see [limits.md](limits.md).

## Linux instances (types `default` and `vm`)

Not yet measured. [vm-instances.md](vm-instances.md) describes a container start as "about a second" and a Firecracker boot as "a few seconds"; those are descriptions from development, not measurements, and belong in the table below once they are.

| Operation | `default` (gVisor) | `vm` (Firecracker) |
|---|---|---|
| Create to `running` (image already pulled) | not yet measured | about 17 s on the Azure VM (rootfs built from the image, kernel boot, gateway up) |
| Wake from sleep (fresh boot, disk intact) | not yet measured | not applicable: `vm` wakes from a snapshot (a cold boot only when the snapshot is unusable) |
| Sleep with memory (pause, full snapshot: a memory file as large as the guest's RAM) | with `--docker-checkpoints` on Docker 27.5.1 (experimental, runsc): the daemon checkpointed a `warren-base` instance in 211 ms; `docker checkpoint create` by hand 0.19 to 0.28 s, 1.4 MB written for `warren-base`, 248 KB for a one-process alpine container (gVisor saves touched pages only); `runsc checkpoint` driven directly 0.04 s | 24 to 25 s for a 4 GB guest on the Azure VM (writing the 4 GB memory file dominates) |
| Wake from the snapshot (processes and pids kept) | Docker 27.5.1: the daemon restored the `warren-base` instance in 462 ms with the gateway answering through the edge right after, the marker process kept its pid; `docker start --checkpoint` by hand 0.32 to 0.34 s; `runsc restore` driven directly 0.08 s. Docker 28 and later fail the restore inside the engine and the daemon boots cold instead ([self-hosting.md](self-hosting.md#sleep-with-memory-on-default-instances)) | 0.2 s to a reachable gateway (204 and 227 ms measured); the marker process kept its pid |
| Fork | not yet measured | not yet measured |
| Exec round trip (`true`) | not yet measured | not yet measured |
| Host memory per idle instance | not yet measured | not yet measured |

On a Mac host the `default` type runs through a smolvm engine instead of Docker Desktop; those figures (first `docker run alpine` 1.3 s warm, `warren/base` booting as a microVM in 7.6 s including the engine adapter's start, an emulated amd64 `warren-claude` create in 23 s) are in [linux-on-mac.md](linux-on-mac.md#measured-here).

## What to measure next

In rough priority order.

1. **Linux numbers**, with the same method as the Mac: time `scripts/e2e.sh` steps and report create, wake, fork, delete and exec round trip for `default` under `runsc`, `default` under `runc` (to show the gVisor cost) and `vm`. Name the host (CPU, RAM, kernel, Docker and gVisor versions).
2. **Distributions, not ranges.** Twenty or more runs per operation and p50 / p95 / max, on the Mac and on Linux. Cold boot versus resume is the comparison that matters for `mac`: the wake range (5 to 11 s) is wide and we do not know whether the spread comes from the saved-state size, from the DHCP lease or from the host being busy.
3. **Wake-on-request latency through the edge**: time to first byte of a request to a sleeping instance's URL, which is what a user of a preview URL feels.
4. **Memory under load**: host RSS and guest `GET /v1/stats` with two guests running an Xcode build each; the point at which a 16 GiB host swaps.
5. **Fork of a sleeping instance** (the clone should carry the saved state and skip the boot) against fork of a running one.
6. **Disk divergence over time**: how fast an instance's APFS clone grows away from the base image during a working day, since backups and clones share blocks until writes happen.
7. **`lume serve` loss**: how long from killing the server to every instance being marked stopped and the first one woken again.
8. **The question iOS developers will ask**: an `xcodebuild` of a known open-source app inside a `mac` instance against the same build on the host, wall clock.

Record the machine, OS and Lume or Docker versions, the Warren commit, the exact command and the raw per-run numbers with any result you add here; attach the raw numbers to the pull request so the table can be recomputed.

## Final runs on the release build (2026-11-20)

Two consecutive `scripts/mac/e2e.sh --port 7357` runs on the hardened base
image (password rotated, keys-only SSH, Node and the agent CLIs preinstalled)
with the release binaries that include the PTY endpoint, the setup commands
and the egress broker:

| Step | Run 1 | Run 2 |
|---|---|---|
| create to running | 22 s | 21 s |
| sleep | 3 s | 3 s |
| wake, marker pid kept | 5 s | 5 s |
| daemon restart, instance adopted | 6 s | 5 s |
| fork | 29 s | 27 s |
| delete both | 19 s | 19 s |

`warren run --type mac --no-tty -- ...` on a fresh directory (create, push 19
files, run, pull one changed file back) took 20 s end to end; `warren shell`
over the PTY returned the remote exit code. On Linux (Azure VM, 32 cores,
Docker with gVisor) the ten-scenario e2e suite took 90 s and an allowlist
`warren-claude` instance reached an allowed host through the broker with a
`CONNECT 200` while an unlisted host was refused.

On macOS, after allowing `warrend` through the Application Firewall, an
allowlist `mac` instance reached `example.com` through the broker in 0.2 to
0.3 s (HTTP 200) and `api.anthropic.com` (HTTP 401 without a key, so the
route works), an unlisted host got the broker's 403, and a request that
bypassed the proxy was dropped by the guest's pf rules.
