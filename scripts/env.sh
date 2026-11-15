# Source this before building or testing: `source scripts/env.sh`
#
# On the original development host every toolchain cache lives under /data (the
# root disk is nearly full and /tmp is mounted noexec). Elsewhere this script only
# defaults CARGO_TARGET_DIR to the repository's target directory; a plain rustup
# installation works as-is.
_warren_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
if [ -d /data/cargo/bin ] && [ -d /data/rustup ]; then
  export CARGO_HOME=/data/cargo RUSTUP_HOME=/data/rustup
  export PATH=/data/cargo/bin:$PATH
  mkdir -p /data/tmp 2>/dev/null && export TMPDIR=/data/tmp
  [ -d /data/npm-cache ] && export npm_config_cache=/data/npm-cache
fi
[ -d /data/node/v24.22.3/bin ] && export PATH=/data/node/v24.22.3/bin:$PATH
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$_warren_root/target}"
unset _warren_root
