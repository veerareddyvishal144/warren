# Source this before building or testing: `source scripts/env.sh`
# The host's /tmp is mounted noexec and the root disk is nearly full, so every
# toolchain cache lives under /data.
export CARGO_HOME=/data/cargo
export RUSTUP_HOME=/data/rustup
export PATH=/data/cargo/bin:/data/node/v24.22.3/bin:$PATH
export TMPDIR=/data/tmp
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/data/warren/target}
export npm_config_cache=/data/npm-cache
mkdir -p /data/tmp
