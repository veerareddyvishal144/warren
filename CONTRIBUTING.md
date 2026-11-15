# Contributing to Warren

Thanks for helping. The rules for building, testing and structuring code are in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md); read it first. The wire contract is [docs/API.md](docs/API.md) and the architecture is [docs/DESIGN.md](docs/DESIGN.md).

## Before you start

- Open an issue for anything that changes the API or the design; both documents define contracts other components depend on.
- Small fixes (docs, tests, a bug with a clear repro) can go straight to a pull request.

## Working on the code

```bash
source scripts/env.sh                       # toolchain and cache locations
cargo build --workspace
cargo test --workspace --exclude warren-e2e
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

- Rust 1.90 or newer, edition 2024. Dependencies are pinned in the root `Cargo.toml`; do not add or change dependencies without agreement, and build with `--locked`.
- Every fallible function returns `warren_core::Result`. Use the `Error` constructors, never panic on bad input.
- Each crate depends on `warren-core` and nothing else inside the workspace (except the binaries that wire things together).
- Unit tests live next to the code with in-memory fakes for the traits a crate consumes. End-to-end tests live in `crates/warren-e2e` and need Docker and the built images: `scripts/e2e.sh` (or `make e2e`); `scripts/e2e.sh --vm` adds the Firecracker scenario. [docs/TESTING.md](docs/TESTING.md) describes all three layers.
- Logging with `tracing`, doc comments on public items, no `unsafe` without a comment explaining why.

## Pull requests

- Keep one logical change per pull request; describe what changed and why, and how you tested it.
- CI runs `cargo fmt --check`, clippy with `-D warnings`, the unit tests, the image builds and the end-to-end suite. Green CI is required.
- Update the documentation that describes the behaviour you changed (`docs/`, `README.md`) and add a line to `CHANGELOG.md` under "Unreleased".
- By contributing you agree that your contributions are licensed under the Apache License 2.0, like the rest of the project.

## Security issues

Do not open a public issue for a vulnerability; follow [SECURITY.md](SECURITY.md).
