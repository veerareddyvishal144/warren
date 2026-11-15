#!/usr/bin/env bash
# Install gVisor (runsc) from the official release tarball and register it as a
# Docker runtime, so default-type Warren instances run under gVisor.
#
# Usage:
#   sudo scripts/install-gvisor.sh [options]
#
# Options:
#   --release NAME       release directory, "latest" (default) or e.g. release-20261109.0
#   --prefix DIR         where runsc, containerd-shim-runsc-v1 and gvisor-bin/ go (default: /usr/local/bin)
#   --daemon-json PATH   Docker daemon config to update (default: /etc/docker/daemon.json)
#   --workdir DIR        where the tarball is downloaded and unpacked (default: $TMPDIR or /tmp)
#   --no-reload          do not run `systemctl reload docker`
#   --no-smoke           skip `docker run --runtime=runsc alpine uname -r`
#   --dry-run            download, verify, unpack and print what would change; write nothing
#
# Steps:
#   1. download gvisor.tar.bz2 and gvisor.tar.bz2.sha512 from
#      https://storage.googleapis.com/gvisor/releases/release/<release>/<arch>/ and verify the SHA-512
#   2. install runsc, containerd-shim-runsc-v1 and the gvisor-bin/ directory into the prefix
#      (atomic replace, so sandboxes already running keep their old binaries)
#   3. merge {"runtimes": {"runsc": {"path": ..., "runtimeArgs": ["--platform=systrap"]}}}
#      into daemon.json with python3 (or jq), keeping every other key such as data-root
#   4. systemctl reload docker and wait until Docker lists the runtime
#   5. smoke test: docker run --rm --runtime=runsc alpine uname -r must print a gVisor kernel
set -euo pipefail

RELEASE="latest"
PREFIX="/usr/local/bin"
DAEMON_JSON="/etc/docker/daemon.json"
WORKDIR_BASE="${TMPDIR:-/tmp}"
RELOAD=1
SMOKE=1
DRY_RUN=0

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
    case "$1" in
        --release) RELEASE="$2"; shift 2 ;;
        --prefix) PREFIX="$2"; shift 2 ;;
        --daemon-json) DAEMON_JSON="$2"; shift 2 ;;
        --workdir) WORKDIR_BASE="$2"; shift 2 ;;
        --no-reload) RELOAD=0; shift ;;
        --no-smoke) SMOKE=0; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

log() { printf '[install-gvisor] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

ARCH="$(uname -m)"
case "$ARCH" in
    x86_64|aarch64) ;;
    *) die "unsupported architecture $ARCH (gVisor publishes x86_64 and aarch64 builds)" ;;
esac
BASE_URL="https://storage.googleapis.com/gvisor/releases/release/${RELEASE}/${ARCH}"

for tool in curl tar bzip2 sha512sum install; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is required"
done

if [ "$DRY_RUN" = 0 ] && [ "$(id -u)" -ne 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "run as root (sudo not found)"
    # Pass the resolved options explicitly: sudo resets the environment, so
    # TMPDIR would otherwise be lost.
    reexec=(--release "$RELEASE" --prefix "$PREFIX" --daemon-json "$DAEMON_JSON" --workdir "$WORKDIR_BASE")
    [ "$RELOAD" = 0 ] && reexec+=(--no-reload)
    [ "$SMOKE" = 0 ] && reexec+=(--no-smoke)
    log "re-running with sudo"
    exec sudo "$0" "${reexec[@]}"
fi

mkdir -p "$WORKDIR_BASE"
WORK="$(mktemp -d "$WORKDIR_BASE/gvisor-install.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# --- 1. download and verify ---------------------------------------------------
log "downloading $BASE_URL/gvisor.tar.bz2"
curl -fsSL --retry 3 --retry-delay 2 -o "$WORK/gvisor.tar.bz2" "$BASE_URL/gvisor.tar.bz2"
curl -fsSL --retry 3 --retry-delay 2 -o "$WORK/gvisor.tar.bz2.sha512" "$BASE_URL/gvisor.tar.bz2.sha512"
grep -Eq '^[0-9a-f]{128}  gvisor\.tar\.bz2$' "$WORK/gvisor.tar.bz2.sha512" \
    || die "unexpected checksum file format: $(head -c 200 "$WORK/gvisor.tar.bz2.sha512")"
(cd "$WORK" && sha512sum -c --quiet gvisor.tar.bz2.sha512) || die "SHA-512 verification failed"
log "sha512 verified ($(stat -c '%s' "$WORK/gvisor.tar.bz2") bytes)"

mkdir -p "$WORK/x"
tar xjf "$WORK/gvisor.tar.bz2" -C "$WORK/x"
for f in runsc containerd-shim-runsc-v1; do
    [ -f "$WORK/x/$f" ] || die "tarball does not contain $f"
done
[ -d "$WORK/x/gvisor-bin" ] || die "tarball does not contain the gvisor-bin directory"
# The helper binaries are looked up relative to runsc, so they must sit in
# <prefix>/gvisor-bin next to it.
helpers="$(find "$WORK/x/gvisor-bin" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort | paste -sd, -)"
log "tarball contents: runsc, containerd-shim-runsc-v1, gvisor-bin/{$helpers}"

# --- 3 (prepared here). merged daemon.json ------------------------------------
RUNSC_PATH="$PREFIX/runsc"

merge_daemon_json() { # prints the merged config on stdout
    if command -v python3 >/dev/null 2>&1; then
        python3 -I - "$DAEMON_JSON" "$RUNSC_PATH" <<'PY'
import json, os, sys
path, runsc = sys.argv[1], sys.argv[2]
cfg = {}
if os.path.isfile(path) and os.path.getsize(path) > 0:
    with open(path) as f:
        cfg = json.load(f)
if not isinstance(cfg, dict):
    sys.exit(f"{path} is not a JSON object")
runtimes = cfg.setdefault("runtimes", {})
entry = runtimes.get("runsc") or {}
args = [a for a in entry.get("runtimeArgs", []) if not str(a).startswith("--platform")]
entry["path"] = runsc
entry["runtimeArgs"] = ["--platform=systrap"] + args
runtimes["runsc"] = entry
json.dump(cfg, sys.stdout, indent=2)
sys.stdout.write("\n")
PY
    elif command -v jq >/dev/null 2>&1; then
        local input="$DAEMON_JSON"
        if ! [ -s "$DAEMON_JSON" ]; then
            input="$WORK/empty.json"
            echo '{}' > "$input"
        fi
        jq --arg p "$RUNSC_PATH" '
            .runtimes = (.runtimes // {})
            | .runtimes.runsc = ((.runtimes.runsc // {})
                | .path = $p
                | .runtimeArgs = (["--platform=systrap"] + ([.runtimeArgs[]? | select(startswith("--platform") | not)])))
        ' "$input"
    else
        die "python3 or jq is required to update $DAEMON_JSON"
    fi
}

MERGED="$WORK/daemon.json"
merge_daemon_json > "$MERGED"

# --- dry run ends here ---------------------------------------------------------
if [ "$DRY_RUN" = 1 ]; then
    log "dry run: would install into $PREFIX:"
    (cd "$WORK/x" && find . -type f | sed 's#^\./#  #' >&2)
    if ( cd "$WORK/x" && ./runsc --version >/dev/null 2>&1 ); then
        log "runsc runs: $(cd "$WORK/x" && ./runsc --version | head -1)"
    else
        log "note: could not execute the unpacked runsc from $WORK (noexec mount?); it is checked after install"
    fi
    log "dry run: $DAEMON_JSON would become:"
    cat "$MERGED" >&2
    log "dry run: would then run 'systemctl reload docker' and 'docker run --rm --runtime=runsc alpine uname -r'"
    exit 0
fi

# --- 2. install ---------------------------------------------------------------
install_file() { # src dest: atomic replace, 0755 root
    install -m 0755 "$1" "$2.tmp.$$"
    mv -f "$2.tmp.$$" "$2"
}
mkdir -p "$PREFIX/gvisor-bin"
install_file "$WORK/x/runsc" "$PREFIX/runsc"
install_file "$WORK/x/containerd-shim-runsc-v1" "$PREFIX/containerd-shim-runsc-v1"
for f in "$WORK"/x/gvisor-bin/*; do
    install_file "$f" "$PREFIX/gvisor-bin/$(basename "$f")"
done
"$PREFIX/runsc" --version >/dev/null || die "$PREFIX/runsc does not run"
log "installed $("$PREFIX/runsc" --version | head -1) into $PREFIX"

# --- 3. register the runtime --------------------------------------------------
if [ -s "$DAEMON_JSON" ] && cmp -s "$DAEMON_JSON" "$MERGED"; then
    log "$DAEMON_JSON already registers runsc with --platform=systrap"
    changed=0
else
    mkdir -p "$(dirname "$DAEMON_JSON")"
    if [ -s "$DAEMON_JSON" ]; then
        cp -p "$DAEMON_JSON" "$DAEMON_JSON.bak"
        log "backed up previous config to $DAEMON_JSON.bak"
    fi
    install -m 0644 "$MERGED" "$DAEMON_JSON"
    log "wrote $DAEMON_JSON:"
    cat "$DAEMON_JSON" >&2
    changed=1
fi

# --- 4. reload Docker ---------------------------------------------------------
if [ "$RELOAD" = 1 ]; then
    if command -v systemctl >/dev/null 2>&1 && systemctl is-active --quiet docker; then
        log "reloading docker"
        systemctl reload docker
    elif [ "$changed" = 1 ]; then
        log "warning: docker is not managed by systemd here; send SIGHUP to dockerd to pick up the new runtime"
    fi
fi

if command -v docker >/dev/null 2>&1; then
    for _ in $(seq 1 30); do
        if docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -q '"runsc"'; then
            break
        fi
        sleep 1
    done
    docker info --format '{{json .Runtimes}}' 2>/dev/null | grep -q '"runsc"' \
        || die "Docker does not list the runsc runtime yet; check 'journalctl -u docker' and $DAEMON_JSON"
    log "docker lists the runsc runtime"
else
    log "docker CLI not found; skipping the runtime check and smoke test"
    SMOKE=0
fi

# --- 5. smoke test ------------------------------------------------------------
if [ "$SMOKE" = 1 ]; then
    log "smoke test: docker run --rm --runtime=runsc alpine uname -r"
    kernel="$(docker run --rm --runtime=runsc alpine uname -r)"
    case "$kernel" in
        *gvisor*) log "ok: sandbox kernel is $kernel" ;;
        *) die "unexpected kernel '$kernel' from the runsc sandbox (host is $(uname -r))" ;;
    esac
fi

log "done"
