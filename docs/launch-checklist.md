# Launch checklist

What has to be true before Warren is announced, the announcement itself, and the answers to the questions it will get. An item is done when someone has checked it on a machine that is not the development machine. Measured claims in any post must come from [benchmarks.md](benchmarks.md), with the method.

## Pre-launch

### Versions and tags

- [ ] Decide the release version. The plan says **v0.2.0**; `Cargo.toml` says `0.1.0` and `CHANGELOG.md` has both an `[Unreleased]` section and a `[0.1.0] - Unreleased` section. Make `Cargo.toml`, `CHANGELOG.md`, the README's Status section and the docs agree before tagging.
- [ ] Move the `[Unreleased]` changelog entries under the release heading with the date; add the compare links at the bottom.
- [ ] Tag the release and push the tag.

### Release assets

- [ ] Add a release workflow. The repository has only `ci.yml` today (fmt, clippy, unit tests, images, end to end on Linux; a `macos-14` job that type-checks). The release workflow should build and attach, with SHA-256 checksums:
  - `warren`, `warrend`, `warren-gateway` (static musl) and `warren-mcp` for `x86_64-unknown-linux-gnu`;
  - `warren`, `warrend`, `warren-gateway` and `warren-mcp` for `aarch64-apple-darwin`;
  - the template images pushed to a registry, or the instructions to build them.
- [ ] Create the Homebrew tap repository (`homebrew-warren`) with a formula that points at the release assets, and check `brew install <owner>/warren/warren` on a Mac that has never had Warren.
- [ ] Check every asset on a **clean Mac** (Apple silicon, fresh user account): install per [install.md](install.md), install Lume, build the base image with `scripts/mac/build-base.sh`, `warren up`, `warren connect claude`, `warren run -- claude`; then `scripts/mac/e2e.sh --skip-build` against the installed binaries, green.
- [ ] Check every asset on a **clean Linux VM** (Debian 12 and Ubuntu 24.04): install per [install.md](install.md), `scripts/install-gvisor.sh`, `warren up`, create an instance, exec, open the terminal, sleep and wake; then `scripts/e2e.sh`, green.
- [ ] Decide what to say about `vm` (Firecracker): run `scripts/e2e.sh --vm` on a KVM host, or state plainly in the release notes that it is experimental and tested on one machine.
- [ ] Re-run the macOS numbers in [benchmarks.md](benchmarks.md) with the release build and update the table if they moved.

### Documentation

- [ ] Publish `docs/` as a site (GitHub Pages or similar) and update the URLs in `docs/llms.txt` to the site if they change.
- [ ] Every link in [index.md](index.md) resolves, including the pages written for this release: [install.md](install.md), [quickstart.md](quickstart.md), [run.md](run.md), [templates.md](templates.md), [compare.md](compare.md), [limits.md](limits.md), [security-model.md](security-model.md), [benchmarks.md](benchmarks.md).
- [ ] `warren up`, `warren connect` and `warren run` in the README and in [run.md](run.md) match the shipped binary's `--help` output exactly.
- [ ] The egress allowlist and secret broker have landed with their flags and fields in [API.md](API.md) and [security.md](security.md), and the five checks in [security-model.md](security-model.md) pass on the clean Mac and the clean Linux VM. If they do not, the README's Status section and [compare.md](compare.md) must say so.
- [ ] [SECURITY.md](../SECURITY.md): enable GitHub private vulnerability reporting and replace the email placeholder; remove the two OWNER TODO comments.
- [ ] Issue templates (bug, feature, macOS host report with Lume version and `sw_vers`), labels including `good first issue`, Discussions enabled.

### Demo

- [ ] Record the Caret demo: a Caret workspace with `warren.template` set to `warren-macos`, the agent creating a `mac` instance, running `xcodebuild` or the Simulator from exec, the instance sleeping and waking with the build still there. Two to three minutes, no narration needed, captions for each step. Also a 30-second Linux clip: `warren run -- claude` on a Linux host.
- [ ] Cut a GIF of the macOS demo for the README (under 10 MB).

## Announcement

- [ ] Show HN, using one of the titles below and the body as written (edit for truth, not for hype). Post on a weekday morning, US Eastern; be available in the thread for the rest of the day.
- [ ] Community posts, each written for its audience, none copied from another:
  - **Claude Code users** (the `claude-code` GitHub discussions, the Anthropic developer Discord, r/ClaudeAI): the MCP server, `warren connect claude`, `warren run -- claude`, and why `/sandbox` is not the same thing.
  - **Codex users** (OpenAI developer forum, r/OpenAI): the MCP server, the Agent API, running Codex inside an instance.
  - **iOS and macOS developers** (r/iOSProgramming, the Swift forums' related projects category, iOS Dev Weekly submission): real macOS VMs for agents on the Mac you have, Xcode inside, two per Mac, the licence paragraph from [limits.md](limits.md).
  - **MCP and agent builders** (the Model Context Protocol discussions): one MCP server for three isolation types.
  - **Self-hosters** (r/selfhosted): one binary, SQLite, your hardware, no account.
- [ ] One post on the maintainer's own social accounts with the GIF and the repository link.
- [ ] After the first week: collect the questions that were actually asked and add them to the FAQ below.

## Show HN draft

### Title options

1. Show HN: Warren – self-hosted sandboxes for coding agents: Linux containers, microVMs and macOS VMs behind one API
2. Show HN: Warren – give Claude Code or Codex a real macOS VM on your own Mac (Rust, Apache-2.0)
3. Show HN: Warren – gVisor, Firecracker and macOS sandboxes with one API, on hardware you own

### Body

> Warren is a single Rust daemon that gives coding agents persistent computers on hardware you own: gVisor containers and Firecracker microVMs on a Linux box, and real macOS VMs on an Apple silicon Mac, behind one REST API, CLI and MCP server. Each instance has a URL, a files API, exec and an agent endpoint, and sleeps when idle.
>
> On the Mac, sleep keeps memory. On a 16 GiB MacBook Pro I measured 3 to 4 s to suspend, 5 to 11 s to resume with processes intact, and 19 to 23 s from create to usable. Apple allows two macOS guests per Mac, so a Mac is a two-slot node.
>
> I built it because Claude Code's /sandbox only wraps Bash, Docker Sandboxes is Linux-only, and cloud Macs cost about $60 a day on 24-hour leases.
>
> It is 0.1.0, single node, and the macOS part is verified on one machine. Apache-2.0.

(About 150 words. Every number is from [benchmarks.md](benchmarks.md); change the machine description to whatever is true for the person posting.)

## FAQ answers

**Why not Docker Sandboxes?** Docker Sandboxes (`sbx run claude`) is a good answer to one question: run a coding agent in a Linux microVM on a Mac, with a credential proxy and an egress policy. Warren answers a different one. It gives you macOS guests as well as Linux ones, it is a service with an API rather than a CLI wrapper (so a Caret workspace, a CI job or your own backend can create and drive instances), each instance has a URL and a files API and an Agent API, and instances persist, sleep, fork and back up. Docker is ahead on the credential proxy and the egress policy: both arrived in Warren with this release and are new. If you only want Claude Code in a Linux VM on your Mac, Docker Sandboxes is less to set up. Sources: [docs.docker.com/ai/sandboxes](https://docs.docker.com/ai/sandboxes/), [compare.md](compare.md).

**Why not Claude Code's /sandbox (Seatbelt)?** Because it sandboxes the Bash tool and nothing else: MCP servers, hooks and file edits run as you, on your machine, and Anthropic's own docs say it is not sufficient for unattended runs ([code.claude.com/docs/en/sandbox-environments](https://code.claude.com/docs/en/sandbox-environments)). It is a real improvement for interactive use, and Anthropic reports 84% fewer permission prompts with it on ([anthropic.com/engineering/claude-code-sandboxing](https://www.anthropic.com/engineering/claude-code-sandboxing)). Warren puts the whole agent, its tools and everything it installs inside a separate kernel (gVisor's Sentry, a Firecracker guest, or a macOS VM) with a persistent disk, a URL and an API; the two compose, since you can run Claude Code with `/sandbox` on inside a Warren instance.

**Is running macOS in VMs like this allowed?** Apple's macOS licence allows up to two additional copies of macOS as virtual machines on each Apple-branded computer already running macOS, for purposes that include software development and testing ([the SLA](https://www.apple.com/legal/sla/docs/macOSTahoe.pdf)). Running Warren on your own Mac for your own agents is that case. Offering macOS VMs to other people is the hosting case in the same document, which requires among other things that each customer have the whole machine for at least 24 hours; that is why cloud Mac providers bill by the day. Warren does not enforce either reading; read the licence, and if you are building a service, ask a lawyer. Not legal advice. More in [limits.md](limits.md).

**How is this different from E2B, Daytona or Modal?** Those are Linux-only clouds with a per-hour price and sub-3-second starts, and they are production services with support. Warren runs on your hardware, has no price, and adds macOS; it is also slower to start, single node and unreleased. [compare.md](compare.md) has the table. Daytona launched cloud macOS sandboxes on 2026-11-17 at about $60 a day in us-east, if you want macOS without owning a Mac.

**Why Lume and not Tart?** Lume (trycua, MIT) has an HTTP API with suspend, which is what sleep-with-memory needs, and it clones VMs with APFS in zero time. Its downsides are documented in [limits.md](limits.md): all VMs in one process, an unauthenticated local API. Replacing it with a runtime that talks to Virtualization.framework directly is on the roadmap. Tart is excellent CI tooling and is now owned by OpenAI; nothing stops a Tart runtime from being written.

**Does it work on an Intel Mac? On Windows?** No and no. macOS guests need Apple silicon; Linux instances need a Linux host (or Docker Desktop on a Mac, under `runc`). Windows guests are not planned.

**Can I run Linux instances on the Mac host too?** Not in this build: `warren up` on a Mac without Docker runs `mac` instances only. Linux guests through Lume on a Mac are Phase 2.

**How do you make money?** We do not. Warren has no billing, no metering and no hosted tier; it is Apache-2.0 software for your own hardware.

**Is it secure enough for untrusted code?** On Linux under gVisor or Firecracker, that is what the boundary is for, with the caveats in [security-model.md](security-model.md). On a Mac, the VM boundary is strong but Lume's local API is unauthenticated and `deny-all` is not enforced for macOS guests yet, so run it on a Mac that is yours and has no untrusted local users. Read [limits.md](limits.md) and [security-model.md](security-model.md) before deciding.
