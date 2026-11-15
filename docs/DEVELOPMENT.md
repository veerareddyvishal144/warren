# Developing Warren

## Toolchain and environment

```bash
source scripts/env.sh          # Rust toolchain under /data, TMPDIR under /data (the host /tmp is noexec)
cargo build --workspace        # shared target dir: /data/warren/target
cargo test -p warren-core
```

Rust 1.99, edition 2024. The in-instance gateway is built fully static for `x86_64-unknown-linux-musl` so it runs in any Linux image (Debian, Alpine, distroless); install `musl-tools` and `rustup target add x86_64-unknown-linux-musl` once (`make musl-setup`), then `make gateway`. Dependencies are pinned in the root `Cargo.toml` under `[workspace.dependencies]`; every crate's `Cargo.toml` lists what it may use. Do not add dependencies or edit any `Cargo.toml` or `Cargo.lock` without agreement: build with `--locked`. If an API needs something not declared, implement it with `std` or a declared crate, or report the need.

Exact dependency sources are on disk at `/data/cargo/registry/src/*/<crate>-<version>/`. When an API does not match your memory, grep the source of the pinned version instead of guessing; several crates (reqwest 0.13, bollard 0.21, rusqlite 0.43, rand/sha2/hmac/base64 current majors, rmcp 3) changed their APIs recently.

## Working on one crate in parallel with others

Builds in one target directory serialize on a lock. When several people or agents work at once, each uses a private target directory seeded from the shared one:

```bash
source scripts/env.sh
export CARGO_TARGET_DIR=/data/warren/target-<crate>
[ -d "$CARGO_TARGET_DIR" ] || cp -a /data/warren/target "$CARGO_TARGET_DIR"
cargo build --locked -p <crate>
cargo test  --locked -p <crate>
cargo clippy --locked -p <crate> --all-targets -- -D warnings
```

Only touch files inside your crate (and the scripts/docs you own). Other crates may not compile while they are being written; build and test your own crate, not the workspace. `warrend` wires everything and is built by the integrator.

## Contract

- `docs/API.md` is the wire contract. `docs/DESIGN.md` is the architecture. `crates/warren-core/src/` holds the types and traits; read `traits.rs`, `types.rs`, `errors.rs`, `config.rs`, `agentapi.rs` before writing code.
- Every fallible function returns `warren_core::Result<T>` (`warren_core::Error`). Use the constructors (`Error::invalid`, `not_found`, `conflict`, `try_again`, `not_supported`, `insufficient_balance`, `instance_limit`, `internal`). Never panic on bad input.
- Money is integer micros. Timestamps are unix seconds (ms on the Agent API). Ids come from `warren_core::ids`.
- Logging: `tracing`. No `println!` in library crates.
- Tests: unit tests in each crate with in-memory fakes for the traits you consume. Integration tests live in `crates/warren-e2e`.
- Style: `cargo fmt`, `cargo clippy -- -D warnings` clean. Small modules, doc comments on public items, no unsafe without a comment explaining why.

## Host facts that matter

- Docker 29 with the `runsc` (gVisor) runtime registered; `docker run --runtime=runsc alpine uname -r` prints `4.19.0-gvisor`. Default runtime is `runc`.
- Nested KVM works; Firecracker v1.18 and a 6.1 kernel plus an Ubuntu rootfs are in `/data/sandbox-probe/`. `/dev/kvm` is world-rw for this session; vm-type work still needs root for tap devices and NAT.
- `*.localhost` resolves to `::1`; the daemon binds dual-stack, so `http://ab12cd34ef.warren.localhost:7337/` works in curl and browsers on this host.
- The dev account is uid 1000, the same uid as the `agent` user in the images, so bind-mounted homes are writable without chown.
