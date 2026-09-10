#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# Kill one recorded session worker inside a multi-tab GUI, prove that another
# pane keeps its original shell and can accept input, then close both tabs
# through the public action and require normal process cleanup.

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARTIFACT_ROOT=${RIO_ACCEPT_ARTIFACT_ROOT:-"$HOME/dev/rio-agent-artifacts"}
RUN_DIR="$ARTIFACT_ROOT/session-isolation-worker-kill-$(date -u +%Y%m%dT%H%M%SZ)-$$"
CONFIG_DIR="$RUN_DIR/config"
LOG_DIR="$RUN_DIR/logs"
RUNTIME_DIR="$ARTIFACT_ROOT/r-$$"
TMP_DIR="$ARTIFACT_ROOT/t-$$"
RIO_BIN=${RIO_BIN:-"$ROOT/target/debug/rio"}
XVFB=${XVFB:-/usr/bin/Xvfb}
XDPYINFO=${XDPYINFO:-/usr/bin/xdpyinfo}
XDOTTOOL=${XDOTTOOL:-/usr/bin/xdotool}
OPENBOX=${OPENBOX:-/usr/bin/openbox}
IMPORT=${IMPORT:-/usr/bin/import}
SHELL_FIXTURE="$ROOT/scripts/fixtures/session-isolation-ack-shell.sh"
DISPLAY_NUM=${RIO_ACCEPT_X_DISPLAY:-$((840 + ($$ % 50)))}
export DISPLAY=":$DISPLAY_NUM" XDG_RUNTIME_DIR="$RUNTIME_DIR" TMPDIR="$TMP_DIR"

mkdir -p "$CONFIG_DIR/gui/log" "$LOG_DIR" "$RUNTIME_DIR" "$TMP_DIR" "$RUN_DIR/home"
chmod 700 "$RUN_DIR" "$CONFIG_DIR" "$RUNTIME_DIR" "$TMP_DIR"
unset WAYLAND_DISPLAY WAYLAND_SOCKET

PIDS=()
ORPHANS=()
XVFB_PID=
OPENBOX_PID=

trap cleanup EXIT INT TERM

process_gone() {
    local state
    state=$(ps -p "$1" -o stat= 2>/dev/null | tr -d ' ' || true)
    [[ -z "$state" || "$state" == Z* ]]
}

process_live() {
    local state
    state=$(ps -p "$1" -o stat= 2>/dev/null | tr -d ' ' || true)
    [[ -n "$state" && "$state" != Z* ]]
}

wait_process_gone() {
    local pid=$1 deadline=$((SECONDS + 5))
    while ((SECONDS < deadline)); do
        process_gone "$pid" && return 0
        sleep 0.1
    done
    return 1
}

session_workers() {
    local root=$1 pid args
    for pid in "$root" $(owned_descendants "$root"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        [[ "$args" == *"--endpoint"* ]] && printf '%s\n' "$pid"
    done
}

shell_child() {
    local worker=$1 pid args
    for pid in $(owned_descendants "$worker"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        [[ "$args" == *"$SHELL_FIXTURE"* ]] && {
            printf '%s\n' "$pid"
            return 0
        }
    done
    return 1
}

send_line() {
    local window=$1 line=$2
    "$XDOTTOOL" windowactivate --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" windowfocus --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" type --clearmodifiers --delay 2 --window "$window" "$line"
    "$XDOTTOOL" key --window "$window" Return
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

for command in "$XVFB" "$XDPYINFO" "$XDOTTOOL" "$OPENBOX" "$IMPORT" xprop pgrep ps awk grep; do
    require_command "$command"
done
[[ -x "$RIO_BIN" ]] || die "RIO_BIN is not executable: $RIO_BIN"
[[ -x "$SHELL_FIXTURE" ]] || die "shell fixture is not executable: $SHELL_FIXTURE"

printf '%s\n' 'draw-bold-text-with-light-colors = true' 'confirm-before-quit = false' '' '[renderer]' 'use-cpu = true' '' \
    '[developer]' 'log-level = "INFO"' >"$CONFIG_DIR/gui/config.toml"
log "run_dir=$RUN_DIR binary=$RIO_BIN display=$DISPLAY runtime=$RUNTIME_DIR tmpdir=$TMPDIR"
"$XVFB" "$DISPLAY" -screen 0 1600x900x24 -nolisten tcp -ac >"$LOG_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
wait_until "private Xvfb" 15 "\"$XDPYINFO\" -display \"$DISPLAY\" >/dev/null 2>&1"
HOME="$RUN_DIR/home" "$OPENBOX" --sm-disable >"$LOG_DIR/openbox.log" 2>&1 &
OPENBOX_PID=$!
wait_until "private Openbox" 15 "kill -0 $OPENBOX_PID 2>/dev/null"

ack_file="$RUN_DIR/gui-ack.txt"
RIO_CONFIG_HOME="$CONFIG_DIR/gui" RIO_LOG_LEVEL=INFO RIO_ACCEPT_ACK_FILE="$ack_file" \
    "$RIO_BIN" --enable-log-file --title-placeholder rio-worker-kill --app-id rio-worker-kill \
    -e /bin/sh "$SHELL_FIXTURE" >"$LOG_DIR/gui.stdout" 2>&1 &
gui_pid=$!
PIDS+=("$gui_pid")
wait_for_text "$ack_file" READY "initial pane shell"
wait_until "GUI window" 30 "window_for_pid $gui_pid >/dev/null"
gui_window=$(window_for_pid "$gui_pid") || die "GUI window not found"
send_line "$gui_window" pane-a
wait_for_text "$ack_file" 'ACK pane-a' "first pane ACK"

palette_action "$gui_window" "New Tab"
wait_until "second session worker" "10" "[[ \$(session_workers \"$gui_pid\" | wc -l) -ge 2 ]]"
mapfile -t workers < <(session_workers "$gui_pid")
(( ${#workers[@]} >= 2 )) || die "second session worker was not found"
for worker in "${workers[@]}"; do
    shell_child "$worker" >/dev/null || die "shell missing for worker $worker"
done

send_line "$gui_window" pane-b
wait_for_text "$ack_file" 'ACK pane-b' "second pane ACK"
pane_b_shell=$(awk '/ACK pane-b/ {split($3, fields, "="); print fields[2]; exit}' "$ack_file")
[[ "$pane_b_shell" =~ ^[0-9]+$ ]] || die "second pane shell PID not found"
pane_b_worker=$(ps -p "$pane_b_shell" -o ppid= | tr -d ' ')
[[ "$pane_b_worker" =~ ^[0-9]+$ ]] || die "second pane worker PID not found"
pane_a_worker=
for worker in "${workers[@]}"; do
    [[ "$worker" != "$pane_b_worker" ]] && pane_a_worker=$worker
done
[[ "$pane_a_worker" =~ ^[0-9]+$ ]] || die "first pane worker PID not found"
pane_a_shell=$(shell_child "$pane_a_worker") || die "first pane shell PID not found"
ORPHANS+=("$pane_a_worker" "$pane_a_shell" "$pane_b_worker" "$pane_b_shell")
log "panes worker_a=$pane_a_worker shell_a=$pane_a_shell worker_b=$pane_b_worker shell_b=$pane_b_shell"

DISPLAY="$DISPLAY" "$IMPORT" -window "$gui_window" "$RUN_DIR/before-kill.png"

# Pane B is current after New Tab. Kill only its recorded session worker while
# keeping pane A attached to the same GUI.
kill -KILL "$pane_b_worker"
wait_until "isolated pane worker exit" 10 "process_gone $pane_b_worker"
process_live "$gui_pid" || die "GUI exited when isolated worker was killed"

# Close the killed tab while pane A still exists; the last-tab policy would
# intentionally refuse this action if it were the only remaining tab.
"$XDOTTOOL" key --window "$gui_window" ctrl+shift+w
wait_until "killed pane shell cleanup" 15 "process_gone $pane_b_shell"

send_line "$gui_window" pane-a
wait_for_text "$ack_file" "ACK pane-a pid=$pane_a_shell" "other pane fresh ACK after worker kill"
log "isolated worker killed; other pane kept GUI and shell continuity"
DISPLAY="$DISPLAY" "$IMPORT" -window "$gui_window" "$RUN_DIR/after-kill.png"

# Close the remaining tab through the public shell path. The frontend keeps
# the event loop alive until the dropped context's session pump acknowledges
# its close request.
send_line "$gui_window" exit
wait_until "healthy pane shell cleanup" 15 "process_gone $pane_a_shell"
wait_until "healthy worker cleanup" 15 "process_gone $pane_a_worker"
wait_until "GUI normal close" 15 "process_gone $gui_pid"
log "PASS: isolated worker kill preserved another pane and public close cleaned all workers"
