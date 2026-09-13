#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# Kill exactly one Rio GUI while its session worker and shell remain alive,
# recover that session through the public saved-session recovery flow, then quit
# normally and verify the recovered worker closes.  No process lookup here is
# broader than the recorded PID tree for this run.

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARTIFACT_ROOT=${RIO_ACCEPT_ARTIFACT_ROOT:-"$HOME/dev/rio-agent-artifacts"}
RUN_DIR="$ARTIFACT_ROOT/session-isolation-crash-recovery-$(date -u +%Y%m%dT%H%M%SZ)-$$"
CONFIG_DIR="$RUN_DIR/config"
LOG_DIR="$RUN_DIR/logs"
RUNTIME_DIR="$ARTIFACT_ROOT/r-$$"
TMP_DIR="$ARTIFACT_ROOT/t-$$"
RIO_BIN=${RIO_BIN:-"$ROOT/target/debug/rio"}
XVFB=${XVFB:-/usr/bin/Xvfb}
XDPYINFO=${XDPYINFO:-/usr/bin/xdpyinfo}
XDOTTOOL=${XDOTTOOL:-/usr/bin/xdotool}
OPENBOX=${OPENBOX:-/usr/bin/openbox}
SHELL_FIXTURE="$ROOT/scripts/fixtures/session-isolation-ack-shell.sh"
DISPLAY_NUM=${RIO_ACCEPT_X_DISPLAY:-$((800 + ($$ % 90)))}
export DISPLAY=":$DISPLAY_NUM" XDG_RUNTIME_DIR="$RUNTIME_DIR" TMPDIR="$TMP_DIR"

mkdir -p "$CONFIG_DIR/source/log" "$CONFIG_DIR/recovery/log" "$LOG_DIR" "$RUNTIME_DIR" "$TMP_DIR"
chmod 700 "$RUN_DIR" "$CONFIG_DIR" "$RUNTIME_DIR" "$TMP_DIR"
unset WAYLAND_DISPLAY WAYLAND_SOCKET

PIDS=()
ORPHANS=()
XVFB_PID=
OPENBOX_PID=

trap cleanup EXIT INT TERM

session_worker() {
    local root=$1 pid args
    for pid in "$root" $(owned_descendants "$root"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        [[ "$args" == *"--endpoint"* ]] && { printf '%s\n' "$pid"; return 0; }
    done
    return 1
}

shell_child() {
    local worker=$1 pid args
    for pid in $(owned_descendants "$worker"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        [[ "$args" == *"$SHELL_FIXTURE"* ]] && { printf '%s\n' "$pid"; return 0; }
    done
    pgrep -P "$worker" | head -n 1
}

start_gui() {
    local role=$1 ack_file="$RUN_DIR/$1-ack.txt" config="$CONFIG_DIR/$1"
    RIO_CONFIG_HOME="$config" RIO_LOG_LEVEL=INFO RIO_ACCEPT_ACK_FILE="$ack_file" \
        "$RIO_BIN" --enable-log-file --title-placeholder "rio-crash-$role" \
        --app-id "rio-crash-$role" -e /bin/sh "$SHELL_FIXTURE" >"$LOG_DIR/$role.stdout" 2>&1 &
    STARTED_PID=$!
    PIDS+=("$STARTED_PID")
}

wait_for_process_window() {
    local pid=$1 result
    wait_until "window for process $pid" 15 \
        "result=\$(window_for_pid \"$pid\" 2>/dev/null || true); [[ -n \"\$result\" ]]"
    window_for_pid "$pid"
}

palette_action() {
    local window=$1 query=$2
    "$XDOTTOOL" windowactivate --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" windowfocus --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" key --window "$window" ctrl+shift+p
    sleep 0.4
    "$XDOTTOOL" type --clearmodifiers --delay 2 --window "$window" "$query"
    sleep 0.4
    "$XDOTTOOL" key --window "$window" Return
    sleep 0.8
}

for command in "$XVFB" "$XDPYINFO" "$XDOTTOOL" "$OPENBOX" xprop pgrep ps awk grep; do require_command "$command"; done
[[ -x "$RIO_BIN" ]] || die "RIO_BIN is not executable: $RIO_BIN"
[[ -x "$SHELL_FIXTURE" ]] || die "shell fixture is not executable: $SHELL_FIXTURE"
for role in source recovery; do
    printf '%s\n' 'draw-bold-text-with-light-colors = true' 'confirm-before-quit = false' '' \
        '[renderer]' 'use-cpu = true' '' '[developer]' 'log-level = "INFO"' >"$CONFIG_DIR/$role/config.toml"
done

log "run_dir=$RUN_DIR binary=$RIO_BIN display=$DISPLAY runtime=$RUNTIME_DIR tmpdir=$TMPDIR"
"$XVFB" "$DISPLAY" -screen 0 1600x900x24 -nolisten tcp -ac >"$LOG_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
wait_until "private Xvfb" 15 "\"$XDPYINFO\" -display \"$DISPLAY\" >/dev/null 2>&1"
mkdir -p "$RUN_DIR/home"
HOME="$RUN_DIR/home" "$OPENBOX" --sm-disable >"$LOG_DIR/openbox.log" 2>&1 &
OPENBOX_PID=$!
wait_until "private Openbox" 15 "kill -0 $OPENBOX_PID 2>/dev/null && xprop -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1"

start_gui source
source_gui=$STARTED_PID
source_ack="$RUN_DIR/source-ack.txt"
wait_for_text "$source_ack" READY "source shell startup"
source_worker=$(session_worker "$source_gui") || die "source worker not found"
source_shell=$(shell_child "$source_worker") || die "source shell not found"
source_window=$(wait_for_process_window "$source_gui") || die "source window not found"
"$XDOTTOOL" windowactivate --sync "$source_window" 2>/dev/null || true
"$XDOTTOOL" windowfocus --sync "$source_window" 2>/dev/null || true
"$XDOTTOOL" type --clearmodifiers --delay 2 --window "$source_window" a
"$XDOTTOOL" key --window "$source_window" Return
wait_for_text "$source_ack" 'input=a' "source pre-crash ACK"
log "recorded source gui=$source_gui worker=$source_worker shell=$source_shell"

kill -KILL "$source_gui"
wait_until "source GUI exit" 10 "! kill -0 $source_gui 2>/dev/null"
kill -0 "$source_worker" 2>/dev/null || die "source worker exited with GUI"
kill -0 "$source_shell" 2>/dev/null || die "source shell exited with GUI"
ORPHANS+=("$source_worker" "$source_shell")
log "source GUI killed; worker and shell still alive with original PIDs"

start_gui recovery
recovery_gui=$STARTED_PID
recovery_ack="$RUN_DIR/recovery-ack.txt"
wait_for_text "$recovery_ack" READY "recovery GUI shell startup"
recovery_worker=$(session_worker "$recovery_gui") || die "recovery worker not found"
recovery_shell=$(shell_child "$recovery_worker") || die "recovery shell not found"
ORPHANS+=("$recovery_worker" "$recovery_shell")
log "recorded recovery gui=$recovery_gui worker=$recovery_worker shell=$recovery_shell"
recovery_window=$(wait_for_process_window "$recovery_gui") || die "recovery window not found"
palette_action "$recovery_window" "Recover Saved Session"

# Recovery candidates are prepared asynchronously. The application opens the
# saved-session-only list when the probe completes; do not toggle the palette
# again, because Ctrl+Shift+P while that mode is active changes its query.
recovery_log="$CONFIG_DIR/recovery/log/rio.log"
wait_for_text "$recovery_log" 'recovery_targets=1' "recovery target discovery"
"$XDOTTOOL" windowactivate --sync "$recovery_window" 2>/dev/null || true
"$XDOTTOOL" windowfocus --sync "$recovery_window" 2>/dev/null || true
"$XDOTTOOL" key --clearmodifiers --window "$recovery_window" Return
sleep 1

"$XDOTTOOL" windowactivate --sync "$recovery_window" 2>/dev/null || true
"$XDOTTOOL" type --clearmodifiers --delay 2 --window "$recovery_window" recovered
"$XDOTTOOL" key --clearmodifiers --window "$recovery_window" Return
wait_for_text "$source_ack" 'input=recovered' "fresh ACK after recovery"
grep -Fq "pid=$source_shell" "$source_ack" || die "recovery ACK came from a different shell PID"
log "recovered source shell PID $source_shell with fresh ACK"

palette_action "$recovery_window" "Close Split or Tab" || true
wait_until "normal recovered worker cleanup" 15 "! kill -0 $source_worker 2>/dev/null"
wait_until "normal recovered shell cleanup" 15 "! kill -0 $source_shell 2>/dev/null"
if kill -0 "$recovery_gui" 2>/dev/null; then
    if recovery_window=$(window_for_pid "$recovery_gui"); then
        # Closing the recovered tab can destroy the native window before the
        # final Quit keystroke reaches it.  In that race normal cleanup has
        # already happened; the PID wait below remains authoritative.
        palette_action "$recovery_window" Quit || true
    fi
fi
wait_until "normal recovery GUI close" 15 "! kill -0 $recovery_gui 2>/dev/null"
log "PASS: one-GUI crash recovery preserved the worker/shell PID and normal close cleaned it up"
