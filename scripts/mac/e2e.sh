#!/usr/bin/env bash
# End-to-end check of `mac` instances on an Apple silicon Mac: builds warrend
# and the native warren-gateway, starts warrend against the Lume base VM and
# walks the product on real macOS VMs (docs/macos.md, "Acceptance for Phase 1"):
#
#   health (runtime lume) -> signup -> create warren-macos/mac -> running
#   -> health through the edge -> exec sw_vers -> marker process
#   -> sleep (lume savedState true) -> wake (marker still alive, same pid)
#   -> restart warrend (the VM keeps running, the reconciler adopts it, same marker pid, no second VM)
#   -> fork -> VM-to-VM isolation (nc from one guest to the other fails)
#   -> third create is 409 host_capacity_reached -> delete both -> gone from lume ls
#
# Usage: bash scripts/mac/e2e.sh [options]
#   --base-vm NAME       stopped Lume VM built by build-base.sh (default warren-macos-26)
#   --port N             warrend listen and public port (default 7337)
#   --data-dir DIR       warrend data directory, log at DIR/warrend.log (default ./data-mac-e2e)
#   --base-domain NAME   base domain sent in Host headers (default warren.localhost)
#   --keep               leave the instances and warrend running at the end
#   --skip-build         do not run cargo build; use target/release/* as they are
#   --clean-leftovers    stop and delete stale warren-* VMs from an earlier run first
#   --warrend-bin PATH   daemon binary (default $CARGO_TARGET_DIR/release/warrend)
#   --gateway-bin PATH   native gateway (default $CARGO_TARGET_DIR/release/warren-gateway)
#   -h, --help           this text
#
# Requests go to http://127.0.0.1:PORT with a Host header (api.<base>:PORT,
# <id>.<base>:PORT) because macOS's resolver does not resolve *.localhost for
# curl. Needs: lume >= 0.7.2, jq, curl, the Rust toolchain (unless --skip-build).
# Exits non-zero on the first failing step and prints the daemon log tail.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=scripts/env.sh
source "$ROOT/scripts/env.sh"
cd "$ROOT"

log() { printf '[mac-e2e] %s\n' "$*" >&2; }
die() { log "error: $*"; exit 1; }

BASE_VM=warren-macos-26 PORT=7337 DATA=./data-mac-e2e BASE_DOMAIN=warren.localhost
KEEP=0 BUILD=1 CLEAN=0 BIN="" GW=""
while [ $# -gt 0 ]; do
    case "$1" in
        --base-vm) BASE_VM="${2:-}"; shift ;;
        --port) PORT="${2:-}"; shift ;;
        --data-dir) DATA="${2:-}"; shift ;;
        --base-domain) BASE_DOMAIN="${2:-}"; shift ;;
        --keep) KEEP=1 ;;
        --skip-build) BUILD=0 ;;
        --clean-leftovers) CLEAN=1 ;;
        --warrend-bin) BIN="${2:-}"; shift ;;
        --gateway-bin) GW="${2:-}"; shift ;;
        -h|--help) sed -n '2,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) die "unknown option $1 (try --help)" ;;
    esac
    shift
done
case "$PORT" in ''|*[!0-9]*) die "--port must be a number, got '$PORT'" ;; esac

[ "$(uname -s)" = Darwin ] || die "this script runs on the Mac host"
for tool in jq curl lume; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool is not installed or not on PATH$( [ "$tool" = jq ] && printf ' (brew install jq)')"
done

# --- binaries ------------------------------------------------------------------------
if [ "$BUILD" = 1 ]; then
    # rustup installs cargo under ~/.cargo/bin and adds it to the PATH of the
    # user's login shell only; this script runs under bash.
    if ! command -v cargo >/dev/null 2>&1; then
        if [ -f "$HOME/.cargo/env" ]; then
            # shellcheck disable=SC1091
            . "$HOME/.cargo/env"
        elif [ -x "$HOME/.cargo/bin/cargo" ]; then
            PATH="$HOME/.cargo/bin:$PATH"
        fi
    fi
    command -v cargo >/dev/null 2>&1 || die "cargo is not on PATH (install rustup, or pass --skip-build)"
    log "building warrend, warren-gateway and warren (release) into $CARGO_TARGET_DIR"
    cargo build --release --locked -p warrend -p warren-gateway -p warren-cli
fi
BIN="${BIN:-$CARGO_TARGET_DIR/release/warrend}"
GW="${GW:-$CARGO_TARGET_DIR/release/warren-gateway}"
[ -x "$BIN" ] || die "$BIN not found; run without --skip-build or pass --warrend-bin"
[ -x "$GW" ] || die "$GW not found; run without --skip-build or pass --gateway-bin"

# --- preflight -------------------------------------------------------------------------
mkdir -p "$DATA"
DATA="$(cd "$DATA" && pwd)"
LOG="$DATA/warrend.log"
API_HOST="api.$BASE_DOMAIN:$PORT"

lume get "$BASE_VM" -f json >/dev/null 2>&1 || die "base VM '$BASE_VM' does not exist; run scripts/mac/build-base.sh first"
BASE_STATUS="$(lume get "$BASE_VM" -f json | jq -r '(if type=="array" then .[0] else . end) | .status // empty')"
[ "$BASE_STATUS" = stopped ] || die "base VM '$BASE_VM' is '$BASE_STATUS'; it must be stopped (lume stop $BASE_VM)"

if curl -s -m 2 -o /dev/null -H "Host: $API_HOST" "http://127.0.0.1:$PORT/v1/health" 2>/dev/null; then
    die "something already answers on 127.0.0.1:$PORT; stop it or pass --port"
fi

# Every VM name is listed whatever the exact shape of `lume ls -f json`.
lume_vm_names() { lume ls -f json 2>/dev/null | jq -r '.. | objects | select(has("name")) | .name' | sort -u; }
# Instance VMs are warren-<instance id> (a letter and nine lowercase alphanumerics);
# the base VM and other warren-* names are not leftovers.
instance_vm_names() { lume_vm_names | grep -E '^warren-[a-z][a-z0-9]{9}$' | grep -vxF "$BASE_VM" || true; }
LEFTOVERS="$(instance_vm_names)"
if [ -n "$LEFTOVERS" ]; then
    if [ "$CLEAN" = 1 ]; then
        for vm in $LEFTOVERS; do
            log "removing leftover VM $vm"
            if [ "$(lume get "$vm" -f json | jq -r '(if type=="array" then .[0] else . end) | .status // empty')" = running ]; then lume stop "$vm" || true; fi
            lume delete "$vm" --force
        done
    else
        die "stale VMs from an earlier run occupy slots: $(printf '%s ' $LEFTOVERS); rerun with --clean-leftovers or lume delete <vm> --force"
    fi
fi

# --- bookkeeping -----------------------------------------------------------------------
WORK="$(mktemp -d "${TMPDIR:-/tmp}/warren-mac-e2e.XXXXXX")"
STEP_NAMES=() STEP_RESULTS=() STEP_TIMES=()
INSTANCES=()
FAILED=0 WARREND_PID="" KEY="" HTTP_CODE="" BODY=""

record() { STEP_NAMES+=("$1"); STEP_RESULTS+=("$2"); STEP_TIMES+=("${3:-}"); [ "$2" = PASS ] || FAILED=1; }
pass() { record "$1" PASS "${2:-}"; log "PASS $1${2:+ (${2}s)}"; }
fail() {
    record "$1" FAIL
    log "FAIL $1: $2"
    if [ -n "${BODY:-}" ]; then log "last response (HTTP $HTTP_CODE): ${BODY:0:600}"; fi
    exit 1
}

print_table() {
    local i
    echo
    printf '%-34s %-6s %10s\n' STEP RESULT SECONDS
    printf '%-34s %-6s %10s\n' ---------------------------------- ------ ----------
    i=0
    while [ "$i" -lt "${#STEP_NAMES[@]}" ]; do
        printf '%-34s %-6s %10s\n' "${STEP_NAMES[$i]}" "${STEP_RESULTS[$i]}" "${STEP_TIMES[$i]:--}"
        i=$((i + 1))
    done
    echo
}

finish() {
    local rc=$? id
    trap - EXIT
    set +e
    if [ "$FAILED" = 1 ] || [ "$rc" -ne 0 ]; then rc=1; fi
    if [ "$KEEP" = 1 ]; then
        log "--keep: leaving warrend (pid ${WARREND_PID:-?}) and the instances running"
        [ -n "$KEY" ] && log "api key: $KEY"
        [ -n "$WARREND_PID" ] && log "stop with: kill $WARREND_PID"
    else
        for id in ${INSTANCES[@]+"${INSTANCES[@]}"}; do
            log "cleanup: deleting instance $id"
            api DELETE "/v1/instances/$id" >/dev/null 2>&1
        done
        if [ -n "$WARREND_PID" ] && kill -0 "$WARREND_PID" 2>/dev/null; then
            # Give the deletes a moment to tear the VMs down before the daemon goes.
            local waited=0
            while [ "$waited" -lt 60 ] && [ -n "$(instance_vm_names)" ]; do sleep 2; waited=$((waited + 2)); done
            log "stopping warrend (pid $WARREND_PID)"
            kill -TERM "$WARREND_PID" 2>/dev/null
            waited=0
            while kill -0 "$WARREND_PID" 2>/dev/null && [ "$waited" -lt 20 ]; do sleep 1; waited=$((waited + 1)); done
            kill -0 "$WARREND_PID" 2>/dev/null && { log "warrend did not exit; SIGKILL"; kill -KILL "$WARREND_PID" 2>/dev/null; }
        fi
        local left
        left="$(instance_vm_names)"
        [ -n "$left" ] && log "warning: VMs still listed by lume after cleanup: $(printf '%s ' $left)"
    fi
    print_table
    if [ "$rc" -ne 0 ]; then
        echo "RESULT: FAIL"
        if [ -f "$LOG" ]; then echo; echo "last 40 lines of $LOG:"; tail -n 40 "$LOG" | sed 's/^/  /'; fi
    else
        echo "RESULT: PASS"
    fi
    rm -rf "$WORK"
    exit "$rc"
}
trap finish EXIT

# --- http helpers ------------------------------------------------------------------------
# api METHOD PATH [JSON]: Sandbox API call; sets HTTP_CODE and BODY.
api() {
    local method=$1 path=$2 data=${3:-}
    local -a args
    args=(-s -S -o "$WORK/body" -w '%{http_code}' -m 180 -X "$method" -H "Host: $API_HOST")
    [ -n "$KEY" ] && args+=(-H "Authorization: Bearer $KEY")
    [ -n "$data" ] && args+=(-H 'Content-Type: application/json' --data "$data")
    : >"$WORK/body"
    HTTP_CODE="$(curl "${args[@]}" "http://127.0.0.1:$PORT$path" 2>"$WORK/curl.err")" || HTTP_CODE=000
    BODY="$(cat "$WORK/body" 2>/dev/null || true)"
}
# edge ID PATH: request to the instance URL through the edge with the API key.
edge() {
    : >"$WORK/body"
    HTTP_CODE="$(curl -s -S -o "$WORK/body" -w '%{http_code}' -m 180 -H "Host: $1.$BASE_DOMAIN:$PORT" -H "X-Warren-Key: $KEY" "http://127.0.0.1:$PORT$2" 2>"$WORK/curl.err")" || HTTP_CODE=000
    BODY="$(cat "$WORK/body" 2>/dev/null || true)"
}
# jget FILTER: jq -r over BODY, empty on anything that is not JSON.
jget() { printf '%s' "$BODY" | jq -r "$1" 2>/dev/null || true; }
# run_exec ID COMMAND [TIMEOUT_SECONDS]
run_exec() {
    local payload
    payload="$(jq -cn --arg c "$2" --argjson t "${3:-120}" '{command: $c, timeout_seconds: $t}')"
    api POST "/v1/instances/$1/exec" "$payload"
}
# wait_status ID WANT TIMEOUT: prints the seconds it took; fails on timeout or `failed`.
wait_status() {
    local id=$1 want=$2 timeout=$3 start=$SECONDS status
    while :; do
        api GET "/v1/instances/$id"
        status="$(jget '.status // empty')"
        if [ "$status" = "$want" ]; then echo $((SECONDS - start)); return 0; fi
        if [ "$status" = failed ]; then log "instance $id failed: $(jget '.status_reason // empty')"; return 1; fi
        if [ $((SECONDS - start)) -ge "$timeout" ]; then log "timeout: $id is '$status' (HTTP $HTTP_CODE) after ${timeout}s, wanted $want"; return 1; fi
        sleep 2
    done
}
# lume_vm_field VM KEY
lume_vm_field() { lume get "$1" -f json 2>/dev/null | jq -r --arg k "$2" '(if type=="array" then .[0] else . end) | .[$k] | select(. != null)'; }

CREATE_PAYLOAD='{"template":"warren-macos","type":"mac","auto_sleep":false}'

# --- daemon helpers ----------------------------------------------------------------------
# start_warrend: launch the daemon in the background, output appended to LOG; sets WARREND_PID.
start_warrend() {
    "$BIN" --runtime none --mac-base-vm "$BASE_VM" --dev \
        --listen ":$PORT" --public-port "$PORT" --data-dir "$DATA" --base-domain "$BASE_DOMAIN" \
        --gateway-bin "$GW" --log "${WARREN_LOG:-info}" --broker-listen "0.0.0.0:$((PORT + 2))" >>"$LOG" 2>&1 &
    WARREND_PID=$!
}
# wait_healthy STEP: poll GET /v1/health until it answers 200 with healthy=true; fails STEP after 120 s.
wait_healthy() {
    local step=$1 start=$SECONDS
    while :; do
        kill -0 "$WARREND_PID" 2>/dev/null || fail "$step" "warrend exited during startup"
        api GET /v1/health
        [ "$HTTP_CODE" = 200 ] && break
        [ $((SECONDS - start)) -lt 120 ] || fail "$step" "no answer from GET /v1/health within 120 s"
        sleep 1
    done
    [ "$(jget .healthy)" = true ] || fail "$step" "healthy is not true"
}
# stop_warrend STEP: SIGTERM the daemon and wait for it to exit; not exiting within 20 s fails STEP.
stop_warrend() {
    local step=$1 waited=0
    kill -TERM "$WARREND_PID" 2>/dev/null || fail "$step" "warrend (pid $WARREND_PID) is not running"
    while kill -0 "$WARREND_PID" 2>/dev/null; do
        if [ "$waited" -ge 20 ]; then
            kill -KILL "$WARREND_PID" 2>/dev/null
            fail "$step" "warrend did not exit within 20 s of SIGTERM"
        fi
        sleep 1
        waited=$((waited + 1))
    done
}

# --- start warrend ------------------------------------------------------------------------
log "starting warrend on :$PORT (base VM $BASE_VM, data $DATA, log $LOG)"
: >"$LOG"
start_warrend

STEP="health (runtime lume)"
start=$SECONDS
wait_healthy "$STEP"
RUNTIMES="$(jget '.runtimes | join(",")')"
case ",$RUNTIMES," in
    *,lume,*) ;;
    *) fail "$STEP" "runtimes is [$RUNTIMES], expected it to contain lume (is $BASE_VM stopped and lume serve reachable? see $LOG)" ;;
esac
pass "$STEP" $((SECONDS - start))
log "warrend $(jget .version), runtimes [$RUNTIMES]"

STEP="signup"
api POST /v1/signup '{"name":"mac-e2e"}'
[ "$HTTP_CODE" = 201 ] || fail "$STEP" "expected 201, got $HTTP_CODE"
KEY="$(jget '.api_key // empty')"
[ -n "$KEY" ] || fail "$STEP" "no api_key in the answer"
pass "$STEP"

STEP="create mac instance -> running"
start=$SECONDS
api POST /v1/instances "$CREATE_PAYLOAD"
[ "$HTTP_CODE" = 201 ] || fail "$STEP" "expected 201, got $HTTP_CODE"
ID="$(jget '.id // empty')"
[ -n "$ID" ] || fail "$STEP" "no id in the answer"
INSTANCES+=("$ID")
log "instance $ID created (VM warren-$ID); waiting for running"
T_CREATE="$(wait_status "$ID" running 240)" || fail "$STEP" "instance $ID did not reach running"
pass "$STEP" "$T_CREATE"

STEP="gateway health through the edge"
edge "$ID" /v1/health
[ "$HTTP_CODE" = 200 ] || fail "$STEP" "expected 200 from http://$ID.$BASE_DOMAIN:$PORT/v1/health, got $HTTP_CODE"
[ "$(jget .healthy)" = true ] || fail "$STEP" "healthy is not true"
log "gateway health: $(printf '%s' "$BODY" | jq -c '{healthy, version, home_owner}' 2>/dev/null || true)"
pass "$STEP"

STEP="exec sw_vers && whoami"
run_exec "$ID" 'sw_vers && whoami'
[ "$HTTP_CODE" = 200 ] || fail "$STEP" "expected 200, got $HTTP_CODE"
[ "$(jget .exit_code)" = 0 ] || fail "$STEP" "exit_code $(jget .exit_code), stderr: $(jget .stderr)"
OUT="$(jget .stdout)"
case "$OUT" in *macOS*) ;; *) fail "$STEP" "stdout does not mention macOS: $OUT" ;; esac
log "exec output: $(printf '%s' "$OUT" | tr '\n' ' ' | tr -s '\t ' ' ')"
pass "$STEP"

STEP="start marker process"
# nohup on purpose, the way an agent starts a server: Apple's nohup fails with
# "can't detach from console" when the gateway runs in launchd's system domain
# (the base image used to do that), so this doubles as the regression check.
# The shell running pgrep would itself match 'sleep 100000'; the [s] trick keeps
# the pattern from matching its own command line.
run_exec "$ID" 'nohup sleep 100000 </dev/null >/dev/null 2>&1 & echo started'
[ "$HTTP_CODE" = 200 ] && [ "$(jget .exit_code)" = 0 ] || fail "$STEP" "exec failed (HTTP $HTTP_CODE)"
case "$(jget .stdout)" in *started*) ;; *) fail "$STEP" "no 'started' in stdout" ;; esac
run_exec "$ID" "pgrep -f '[s]leep 100000'"
PID_BEFORE="$(jget .stdout | tr -d '[:space:]')"
if [ -z "$PID_BEFORE" ]; then
    run_exec "$ID" 'echo "launchd domain: $(launchctl managername 2>&1)"; nohup true; echo "nohup rc=$?"'
    fail "$STEP" "marker process not found right after starting it; guest says: $(jget .stdout | tr '\n' ' ')$(jget .stderr | tr '\n' ' ')"
fi
log "marker pid $PID_BEFORE"
pass "$STEP"

STEP="sleep -> sleeping (savedState)"
start=$SECONDS
api POST "/v1/instances/$ID/sleep"
[ "$HTTP_CODE" = 200 ] || fail "$STEP" "expected 200, got $HTTP_CODE"
wait_status "$ID" sleeping 120 >/dev/null || fail "$STEP" "instance did not reach sleeping"
T_SLEEP=$((SECONDS - start))
SAVED="$(lume_vm_field "warren-$ID" savedState)"
[ "$SAVED" = true ] || fail "$STEP" "lume get warren-$ID reports savedState '$SAVED', expected true"
pass "$STEP" "$T_SLEEP"

STEP="wake -> running (marker alive)"
start=$SECONDS
api POST "/v1/instances/$ID/start"
[ "$HTTP_CODE" = 200 ] || fail "$STEP" "expected 200, got $HTTP_CODE"
wait_status "$ID" running 120 >/dev/null || fail "$STEP" "instance did not reach running"
T_WAKE=$((SECONDS - start))
run_exec "$ID" "pgrep -f '[s]leep 100000'"
[ "$HTTP_CODE" = 200 ] || fail "$STEP" "exec after wake failed (HTTP $HTTP_CODE)"
PID_AFTER="$(jget .stdout | tr -d '[:space:]')"
[ -n "$PID_AFTER" ] || fail "$STEP" "marker process is gone after wake (memory was not preserved)"
[ "$PID_AFTER" = "$PID_BEFORE" ] || fail "$STEP" "marker pid changed from $PID_BEFORE to $PID_AFTER (the VM rebooted instead of resuming)"
pass "$STEP" "$T_WAKE"

STEP="restart warrend -> instance adopted"
# The daemon leaves VMs running when it exits (they live in the lume serve
# process, which it never stops) and its reconciler adopts them on the next
# start: same VM, same processes inside, no second clone.
start=$SECONDS
LOG_LINES_BEFORE="$(wc -l <"$LOG" | tr -d ' ')"
stop_warrend "$STEP"
VMS="$(instance_vm_names | tr '\n' ' ')"
[ "$VMS" = "warren-$ID " ] || fail "$STEP" "after the daemon exit lume lists [$VMS], expected only warren-$ID"
[ "$(lume_vm_field "warren-$ID" status)" = running ] || fail "$STEP" "VM warren-$ID is not running after the daemon exit"
log "warrend exited and VM warren-$ID is still running; starting warrend again"
start_warrend
wait_healthy "$STEP"
ADOPTED=0
for _ in $(seq 1 30); do
    if tail -n "+$((LOG_LINES_BEFORE + 1))" "$LOG" | grep -q "adopted running instance.*$ID"; then ADOPTED=1; break; fi
    sleep 1
done
[ "$ADOPTED" = 1 ] || fail "$STEP" "no 'adopted running instance' line for $ID in $LOG within 30 s of the restart"
api GET "/v1/instances/$ID"
[ "$HTTP_CODE" = 200 ] || fail "$STEP" "GET instance after the restart: HTTP $HTTP_CODE"
[ "$(jget .status)" = running ] || fail "$STEP" "instance is '$(jget .status)' after the restart ($(jget .status_reason))"
edge "$ID" /v1/health
[ "$HTTP_CODE" = 200 ] || fail "$STEP" "gateway health through the edge after the restart: HTTP $HTTP_CODE"
run_exec "$ID" "pgrep -f '[s]leep 100000'"
[ "$HTTP_CODE" = 200 ] || fail "$STEP" "exec after the restart failed (HTTP $HTTP_CODE)"
PID_ADOPTED="$(jget .stdout | tr -d '[:space:]')"
[ "$PID_ADOPTED" = "$PID_BEFORE" ] || fail "$STEP" "marker pid is '$PID_ADOPTED' after the restart, was $PID_BEFORE (the VM was rebooted or re-created)"
VMS="$(instance_vm_names | tr '\n' ' ')"
[ "$VMS" = "warren-$ID " ] || fail "$STEP" "after the restart lume lists [$VMS], expected only warren-$ID"
T_RESTART=$((SECONDS - start))
log "adopted: VM warren-$ID, marker pid $PID_ADOPTED unchanged"
pass "$STEP" "$T_RESTART"

STEP="fork -> running"
start=$SECONDS
api POST "/v1/instances/$ID/fork" '{"name":"mac-e2e-fork"}'
[ "$HTTP_CODE" = 201 ] || fail "$STEP" "expected 201, got $HTTP_CODE"
FORK="$(jget '.id // empty')"
[ -n "$FORK" ] || fail "$STEP" "no id in the fork answer"
INSTANCES+=("$FORK")
log "fork $FORK created (VM warren-$FORK); waiting for both instances to run"
wait_status "$FORK" running 240 >/dev/null || fail "$STEP" "fork $FORK did not reach running"
# A running source is slept for the copy and woken afterwards.
wait_status "$ID" running 120 >/dev/null || fail "$STEP" "source $ID did not return to running after the fork"
T_FORK=$((SECONDS - start))
pass "$STEP" "$T_FORK"

STEP="pf isolation (guest -> guest blocked)"
FORK_IP="$(lume_vm_field "warren-$FORK" ipAddress)"
[ -n "$FORK_IP" ] || fail "$STEP" "lume get warren-$FORK has no ipAddress"
# Positive control: the host reaches the fork's gateway port (pf allows the gateway address).
if ! nc -z -w 3 "$FORK_IP" 3737 >/dev/null 2>&1; then
    fail "$STEP" "the host cannot reach $FORK_IP:3737 either, so the guest-side check would prove nothing"
fi
# pf drops the SYN silently, so connect() would wait on TCP retransmits for
# over a minute; macOS nc bounds the connect with -G (-w only covers idle time).
run_exec "$ID" "nc -z -G 3 -w 3 $FORK_IP 3737; echo rc=\$?" 30
[ "$HTTP_CODE" = 200 ] || fail "$STEP" "exec failed (HTTP $HTTP_CODE)"
OUT="$(jget .stdout)"
RC="$(printf '%s' "$OUT" | sed -n 's/.*rc=\([0-9][0-9]*\).*/\1/p' | tail -n 1)"
[ -n "$RC" ] || fail "$STEP" "no rc= in the output: $OUT"
[ "$RC" != 0 ] || fail "$STEP" "instance $ID reached the fork at $FORK_IP:3737 (rc=0); the pf anchor is not blocking VM-to-VM traffic"
log "nc from $ID to $FORK_IP:3737 -> rc=$RC (blocked)"
pass "$STEP"

STEP="third create -> 409 host_capacity_reached"
api POST /v1/instances "$CREATE_PAYLOAD"
if [ "$HTTP_CODE" = 201 ]; then
    THIRD="$(jget '.id // empty')"
    [ -n "$THIRD" ] && INSTANCES+=("$THIRD")
    fail "$STEP" "a third instance was created ($THIRD) although two macOS guests already run"
fi
[ "$HTTP_CODE" = 409 ] || fail "$STEP" "expected 409, got $HTTP_CODE"
CODE="$(jget '.error.code // empty')"
[ "$CODE" = host_capacity_reached ] || fail "$STEP" "error.code is '$CODE', expected host_capacity_reached"
log "details: $(jget '.error.details // {}' | tr -d '\n')"
pass "$STEP"

STEP="delete both -> gone from lume ls"
start=$SECONDS
for id in "$ID" "$FORK"; do
    api DELETE "/v1/instances/$id"
    [ "$HTTP_CODE" = 200 ] || fail "$STEP" "DELETE $id: expected 200, got $HTTP_CODE"
done
for id in "$ID" "$FORK"; do
    while :; do
        api GET "/v1/instances/$id"
        [ "$HTTP_CODE" = 404 ] && break
        [ $((SECONDS - start)) -lt 120 ] || fail "$STEP" "$id still answers HTTP $HTTP_CODE ($(jget '.status // empty')) 120 s after delete"
        sleep 2
    done
done
INSTANCES=()
while :; do
    NAMES="$(lume_vm_names || true)"
    case "$NAMES" in
        *"warren-$ID"*|*"warren-$FORK"*)
            [ $((SECONDS - start)) -lt 180 ] || fail "$STEP" "lume ls still lists a VM of a deleted instance: $(printf '%s ' $NAMES)"
            sleep 2 ;;
        *) break ;;
    esac
done
T_DELETE=$((SECONDS - start))
pass "$STEP" "$T_DELETE"

echo
echo "timings: create->running ${T_CREATE}s, sleep ${T_SLEEP}s, wake ${T_WAKE}s, daemon restart ${T_RESTART}s, fork ${T_FORK}s, delete ${T_DELETE}s"
exit 0
