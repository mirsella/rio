#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# Interrupt a selected public tab merge before the target click. The retained
# tab and destination shell must remain usable, while the killed tab must not
# be acknowledged a second time or become a duplicate owner.

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARTIFACT_ROOT=${RIO_ACCEPT_ARTIFACT_ROOT:-"$HOME/dev/rio-agent-artifacts"}
RUN_DIR="$ARTIFACT_ROOT/session-isolation-partial-transfer-$(date -u +%Y%m%dT%H%M%SZ)-$$"
CONFIG_DIR="$RUN_DIR/config"
LOG_DIR="$RUN_DIR/logs"
RUNTIME_DIR="$ARTIFACT_ROOT/r-$$"
TMP_DIR="$ARTIFACT_ROOT/t-$$"
DISPLAY_NUM=${RIO_ACCEPT_X_DISPLAY:-$((920 + ($$ % 40)))}
RIO_BIN=${RIO_BIN:-"$ROOT/target/debug/rio"}
XVFB=${XVFB:-/usr/bin/Xvfb}
XDPYINFO=${XDPYINFO:-/usr/bin/xdpyinfo}
XDOTTOOL=${XDOTTOOL:-/usr/bin/xdotool}
IMPORT=${RIO_ACCEPT_IMPORT:-/usr/bin/import}
SHELL_FIXTURE="$ROOT/scripts/fixtures/session-isolation-ack-shell.sh"
export DISPLAY=":$DISPLAY_NUM" XDG_RUNTIME_DIR="$RUNTIME_DIR" TMPDIR="$TMP_DIR"
unset WAYLAND_DISPLAY WAYLAND_SOCKET

mkdir -p "$CONFIG_DIR/source/log" "$CONFIG_DIR/destination/log" \
    "$LOG_DIR" "$RUNTIME_DIR" "$TMP_DIR"
chmod 700 "$RUN_DIR" "$CONFIG_DIR" "$RUNTIME_DIR" "$TMP_DIR"

PIDS=()
ORPHANS=()
XVFB_PID=

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

session_workers() {
    local root=$1 pid args
    for pid in "$root" $(owned_descendants "$root"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        [[ "$args" == *"--endpoint"* ]] && printf '%s\n' "$pid"
    done
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

start_gui() {
    local role=$1 title=$2 ack=$3 stdout=$4
    RIO_CONFIG_HOME="$CONFIG_DIR/$role" RIO_LOG_LEVEL=INFO RIO_ACCEPT_ACK_FILE="$ack" \
        "$RIO_BIN" --enable-log-file --title-placeholder "$title" \
        --app-id "rio-partial-$role" -e /bin/sh "$SHELL_FIXTURE" \
        >"$stdout" 2>&1 &
    STARTED_PID=$!
    PIDS+=("$STARTED_PID")
}

for command in "$XVFB" "$XDPYINFO" "$XDOTTOOL" "$IMPORT" xprop pgrep ps awk grep tr; do
    require_command "$command"
done
[[ -x "$RIO_BIN" ]] || die "RIO_BIN is not executable: $RIO_BIN"
[[ -x "$SHELL_FIXTURE" ]] || die "shell fixture is not executable: $SHELL_FIXTURE"

printf '%s\n' \
    'draw-bold-text-with-light-colors = true' \
    'confirm-before-quit = false' \
    '' '[renderer]' 'use-cpu = true' \
    '' '[developer]' 'log-level = "INFO"' \
    >"$CONFIG_DIR/destination/config.toml"
printf '%s\n' \
    "shell = { program = \"/bin/sh\", args = [\"$SHELL_FIXTURE\"] }" \
    'draw-bold-text-with-light-colors = true' \
    'confirm-before-quit = false' \
    '' '[renderer]' 'use-cpu = true' \
    '' '[developer]' 'log-level = "INFO"' \
    >"$CONFIG_DIR/source/config.toml"

log "run_dir=$RUN_DIR binary=$RIO_BIN display=$DISPLAY"
"$XVFB" "$DISPLAY" -screen 0 1600x900x24 -nolisten tcp -ac \
    >"$LOG_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
PIDS+=("$XVFB_PID")
wait_until "private Xvfb" 15 "\"$XDPYINFO\" -display \"$DISPLAY\" >/dev/null 2>&1"

source_ack="$RUN_DIR/source.ack"
destination_ack="$RUN_DIR/destination.ack"
source_rio_log="$CONFIG_DIR/source/log/rio.log"
destination_rio_log="$CONFIG_DIR/destination/log/rio.log"
start_gui source rio-partial-source "$source_ack" "$LOG_DIR/source.stdout"
source_pid=$STARTED_PID
start_gui destination rio-partial-destination "$destination_ack" "$LOG_DIR/destination.stdout"
destination_pid=$STARTED_PID
wait_until "source window" 30 "window_for_pid $source_pid >/dev/null"
wait_until "destination window" 30 "window_for_pid $destination_pid >/dev/null"
source_window=$(window_for_pid "$source_pid")
destination_window=$(window_for_pid "$destination_pid")
"$XDOTTOOL" windowmove "$source_window" 0 0
"$XDOTTOOL" windowsize "$source_window" 800 490
"$XDOTTOOL" windowmove "$destination_window" 800 0
"$XDOTTOOL" windowsize "$destination_window" 800 490
wait_for_text "$source_ack" READY "source shell"
wait_for_text "$destination_ack" READY "destination shell"

source_shell_pid=$(awk -F'[ =]' '/^READY / {print $3; exit}' "$source_ack")
destination_shell_pid=$(awk -F'[ =]' '/^READY / {print $3; exit}' "$destination_ack")
send_line "$source_window" tab-keep
send_line "$destination_window" ack
wait_for_text "$source_ack" "ACK pid=$source_shell_pid input=tab-keep" "retained source shell"
wait_for_text "$destination_ack" "ACK pid=$destination_shell_pid count=1" "destination shell"

palette_action "$source_window" "New Tab"
wait_until "selected tab shell" 10 \
    "(( \$(grep -Fc READY \"$source_ack\") >= 2 ))"
selected_shell_pid=$(awk -F'[ =]' '/^READY / {pid=$3} END {print pid}' "$source_ack")
[[ "$selected_shell_pid" =~ ^[0-9]+$ && "$selected_shell_pid" != "$source_shell_pid" ]] \
    || die "selected tab shell was not distinct from retained source shell"
send_line "$source_window" tab-transfer
wait_for_text "$source_ack" "ACK pid=$selected_shell_pid input=tab-transfer" "selected tab shell"
selected_worker_pid=$(ps -p "$selected_shell_pid" -o ppid= | tr -d ' ')
[[ "$selected_worker_pid" =~ ^[0-9]+$ ]] || die "selected tab worker was not found"
worker_args=$(ps -p "$selected_worker_pid" -o args= 2>/dev/null || true)
[[ "$worker_args" == *"--endpoint"* ]] || die "selected tab parent is not a session worker"
ORPHANS+=("$selected_worker_pid" "$selected_shell_pid")
ready_count=$(grep -Fc READY "$source_ack")
log "source retained_shell=$source_shell_pid selected_shell=$selected_shell_pid selected_worker=$selected_worker_pid ready_count=$ready_count"

palette_action "$source_window" "Merge Tab"
sleep 1
"$XDOTTOOL" windowraise "$destination_window" 2>/dev/null || true
"$XDOTTOOL" windowfocus --sync "$destination_window" 2>/dev/null || true
"$XDOTTOOL" windowactivate --sync "$destination_window" 2>/dev/null || true
"$XDOTTOOL" mousemove --window "$destination_window" 120 20
wait_until "partial merge target arm" 8 \
    "[[ -f \"$destination_rio_log\" ]] && grep -Eq 'native merge target pointer (entered|moved)' \"$destination_rio_log\""
DISPLAY="$DISPLAY" "$IMPORT" -window root "$RUN_DIR/before-interrupt.png"

kill -KILL "$selected_worker_pid"
wait_until "selected worker interruption" 10 "process_gone $selected_worker_pid"
wait_until "selected shell interruption" 10 "process_gone $selected_shell_pid"
process_live "$source_pid" || die "source GUI exited when selected worker was interrupted"

"$XDOTTOOL" click --clearmodifiers 1
sleep 2
process_live "$source_pid" || die "source GUI exited after interrupted merge click"
process_live "$destination_pid" || die "destination GUI exited after interrupted merge click"
wait_for_text "$source_rio_log" "arming native merge targets" "source interrupted merge state"
"$XDOTTOOL" key --window "$source_window" --clearmodifiers super+1
send_line "$source_window" tab-keep
send_line "$destination_window" ack
wait_for_text "$source_ack" "ACK pid=$source_shell_pid input=tab-keep" "retained source shell after interruption"
wait_for_text "$destination_ack" "ACK pid=$destination_shell_pid count=2" "destination shell after interruption"
[[ $(grep -Fc "ACK pid=$selected_shell_pid input=tab-transfer" "$source_ack") == 1 ]] \
    || die "interrupted selected tab produced a duplicate ACK"
DISPLAY="$DISPLAY" "$IMPORT" -window root "$RUN_DIR/after-interrupt.png"
log "PASS: interrupted selected-tab transfer preserved retained source and destination control without duplicate selected ACKs"
