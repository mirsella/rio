#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# Run native Wayland Rio surfaces under KWin's X11 backend, hosted by a private
# Xvfb display. xdotool injects XTest events into that private host only.
# This is intentionally CPU presentation: the X11-backed KWin surface can lose
# a Vulkan swapchain. It exercises direct resident Sugarloaf rendering and
# target-local native Wayland events.

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARTIFACT_ROOT=${RIO_ACCEPT_ARTIFACT_ROOT:-"$HOME/dev/rio-agent-artifacts"}
RUN_ID="session-isolation-wayland-x11-pointer-$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_DIR="$ARTIFACT_ROOT/$RUN_ID"
CONFIG_DIR="$RUN_DIR/config"
LOG_DIR="$RUN_DIR/logs"
RUNTIME_DIR="$ARTIFACT_ROOT/wx-$$"
TMP_DIR="$ARTIFACT_ROOT/tx-$$"

RIO_BIN=${RIO_BIN:-"$ROOT/target/debug/rio"}
XVFB=${XVFB:-/usr/bin/Xvfb}
KWIN=${KWIN:-/usr/bin/kwin_wayland}
DBUS_RUN_SESSION=${DBUS_RUN_SESSION:-/usr/bin/dbus-run-session}
DBUS_DAEMON=${DBUS_DAEMON:-/usr/bin/dbus-daemon}
OPENBOX=${OPENBOX:-/usr/bin/openbox}
XDPYINFO=${XDPYINFO:-/usr/bin/xdpyinfo}
XDOTTOOL=${XDOTTOOL:-/usr/bin/xdotool}
XPROP=${XPROP:-/usr/bin/xprop}
QDBUS6=${QDBUS6:-/usr/bin/qdbus6}
WL_COPY=${WL_COPY:-/usr/bin/wl-copy}
SHELL_FIXTURE="$ROOT/scripts/fixtures/session-isolation-ack-shell.sh"
SETXKBMAP=${SETXKBMAP:-/usr/bin/setxkbmap}
IMPORT=${IMPORT:-/usr/bin/import}
COMPARE=${COMPARE:-/usr/bin/compare}
SOCKET=${RIO_ACCEPT_WAYLAND_SOCKET:-rio-accept-wayland-x11}
DISPLAY_NUM=${RIO_ACCEPT_X_DISPLAY:-$((300 + ($$ % 300)))}
BUS_ADDRESS="unix:path=$RUNTIME_DIR/bus"
export DISPLAY=":$DISPLAY_NUM"
export XDG_RUNTIME_DIR="$RUNTIME_DIR"
export TMPDIR="$TMP_DIR"

mkdir -p "$CONFIG_DIR/source" "$CONFIG_DIR/destination" "$LOG_DIR" "$RUNTIME_DIR" "$TMP_DIR"
mkdir -p "$RUN_DIR/home" "$RUN_DIR/kwin-config" "$RUN_DIR/kwin-data" "$RUN_DIR/kwin-cache"
chmod 700 "$RUN_DIR" "$CONFIG_DIR" "$RUNTIME_DIR" "$TMP_DIR"
unset WAYLAND_DISPLAY WAYLAND_SOCKET

PIDS=()
DBUS_PID=
OPENBOX_PID=
CLIPBOARD_PID=

trap cleanup EXIT INT TERM

rio_pid() {
    local root=$1 pid args
    for pid in "$root" $(owned_descendants "$root"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        if [[ "$args" == *"$RIO_BIN --enable-log-file"* ]]; then
            printf '%s\n' "$pid"
            return 0
        fi
    done
    return 1
}

wait_for_log() {
    local description=$1 file=$2 text=$3
    wait_until "$description" 15 "[[ -f \"$file\" ]] && grep -Fq \"$text\" \"$file\""
}

capture_root() {
    local output=$1
    "$IMPORT" -window root "$output" >/dev/null 2>&1 || die "could not capture private X11 root"
    [[ -s "$output" ]] || die "private root capture was empty: $output"
}

image_changed() {
    local first=$1 second=$2 metric
    metric=$("$COMPARE" -metric AE "$first" "$second" null: 2>&1 || true)
    metric=${metric%%$'\n'*}
    metric=${metric%% *}
    metric=${metric%%.*}
    [[ "$metric" =~ ^[0-9]+$ ]] && ((metric >= ${RIO_ACCEPT_MIN_IMAGE_DELTA:-1000}))
}

palette_action() {
    local query=$1
    "$XDOTTOOL" key --clearmodifiers ctrl+shift+p
    sleep 0.5
    "$XDOTTOOL" type --clearmodifiers --delay 2 "$query"
    sleep 0.5
    "$XDOTTOOL" key --clearmodifiers Return
    sleep 1
}

for command in "$XVFB" "$KWIN" "$DBUS_DAEMON" "$OPENBOX" "$XDPYINFO" "$XDOTTOOL" "$XPROP" "$QDBUS6" "$WL_COPY" "$SETXKBMAP" "$IMPORT" "$COMPARE" pgrep ps awk grep tr; do
    require_command "$command"
done
[[ -x "$RIO_BIN" ]] || die "RIO_BIN is not executable: $RIO_BIN"
[[ -x "$SHELL_FIXTURE" ]] || die "shell fixture is not executable: $SHELL_FIXTURE"

log "run_dir=$RUN_DIR"
log "binary=$RIO_BIN"
log "display=$DISPLAY"
log "runtime=$RUNTIME_DIR"
log "tmpdir=$TMPDIR"
log "wayland_socket=$SOCKET"
log "kwin_backend=x11"
log "renderer.use-cpu=1"
log "input=private XTest pointer, keyboard palette, and private Wayland primary selection"

"$XVFB" "$DISPLAY" -screen 0 1600x1000x24 -nolisten tcp -ac >"$LOG_DIR/xvfb.log" 2>&1 &
xvfb_pid=$!
PIDS+=("$xvfb_pid")
wait_until "private Xvfb host" 15 "\"$XDPYINFO\" -display \"$DISPLAY\" >/dev/null 2>&1"

HOME="$RUN_DIR/home" DISPLAY="$DISPLAY" "$OPENBOX" --sm-disable >"$LOG_DIR/openbox.log" 2>&1 &
OPENBOX_PID=$!
PIDS+=("$OPENBOX_PID")
wait_until "private Openbox" 15 "kill -0 $OPENBOX_PID 2>/dev/null && $XPROP -root _NET_SUPPORTING_WM_CHECK >/dev/null 2>&1"

# KWin inherits the host keymap. Match it on the private Xvfb server before
# starting native clients; otherwise XTest key symbols do not reach Rio.
DISPLAY="$DISPLAY" "$SETXKBMAP" -layout us -variant colemak_dh_iso

DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$DBUS_DAEMON" --session \
    --address="$BUS_ADDRESS" --nofork >"$LOG_DIR/dbus.log" 2>&1 &
DBUS_PID=$!
wait_until "private D-Bus" 15 "kill -0 $DBUS_PID 2>/dev/null"

HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/kwin-config" \
    XDG_DATA_HOME="$RUN_DIR/kwin-data" XDG_CACHE_HOME="$RUN_DIR/kwin-cache" \
    QT_QPA_PLATFORM=xcb LIBGL_ALWAYS_SOFTWARE=1 \
    XDG_RUNTIME_DIR="$RUNTIME_DIR" DISPLAY="$DISPLAY" \
    DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$KWIN" --socket="$SOCKET" \
    --x11-display="$DISPLAY" --width=1600 --height=1000 --no-lockscreen \
    --no-global-shortcuts --no-kactivities \
    >"$LOG_DIR/kwin.log" 2>&1 &
kwin_pid=$!
PIDS+=("$kwin_pid")
wait_until "KWin Wayland socket" 15 "[[ -S \"$RUNTIME_DIR/$SOCKET\" ]] && kill -0 $kwin_pid 2>/dev/null"
log "private KWin X11-backed Wayland compositor ready pid=$kwin_pid"
log "private X11 active_window=$($XDOTTOOL getactivewindow 2>/dev/null || true)"
log "private X11 visible_windows=$($XDOTTOOL search --onlyvisible --name . 2>/dev/null | tr '\n' ' ' || true)"
log "private X11 unnamed_windows=$($XDOTTOOL search --all --name '' 2>/dev/null | tr '\n' ' ' || true)"
client_list=$($XPROP -root _NET_CLIENT_LIST 2>/dev/null || true)
log "private X11 client_list=$client_list"
if [[ "$client_list" =~ (0x[0-9a-fA-F]+) ]]; then
    host_windows=("$((16#${BASH_REMATCH[1]#0x}))")
else
    mapfile -t host_windows < <("$XDOTTOOL" search --onlyvisible --name . 2>/dev/null || true)
fi
(( ${#host_windows[@]} > 0 )) || die "private KWin did not expose an X11 host window"
host_window=$("$XDOTTOOL" getactivewindow 2>/dev/null || true)
if [[ ! "$host_window" =~ ^[0-9]+$ || ! " ${host_windows[*]} " == *" $host_window "* ]]; then
    host_window=${host_windows[0]}
fi
"$XDOTTOOL" windowactivate --sync "$host_window" 2>/dev/null || true
"$XDOTTOOL" windowfocus --sync "$host_window" 2>/dev/null || true
log "private X11 KWin host window=$host_window"
for role in destination source; do
    mkdir -p "$CONFIG_DIR/$role/log"
    printf '%s\n' \
        'draw-bold-text-with-light-colors = true' \
        '' \
        '[renderer]' \
        'use-cpu = true' \
        '' \
        '[developer]' \
        'log-level = "INFO"' \
        >"$CONFIG_DIR/$role/config.toml"
done

start_gui() {
    local role=$1
    local ack_file="$RUN_DIR/$role-ack.txt"
    local config="$CONFIG_DIR/$role"
    XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$SOCKET" \
        RIO_CONFIG_HOME="$config" RIO_LOG_LEVEL=INFO RIO_ACCEPT_ACK_FILE="$ack_file" \
        env -u DISPLAY DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$RIO_BIN" --enable-log-file \
        --title-placeholder "rio-wayland-x11-$role" --app-id "rio-wayland-x11-$role" \
        -e /bin/sh "$SHELL_FIXTURE" >"$LOG_DIR/$role.stdout" 2>&1 &
    STARTED_PID=$!
    PIDS+=("$STARTED_PID")
}

# Start the destination first, then the source so the source owns host focus
# when the public Merge action is injected.
start_gui destination
destination_wrapper=$STARTED_PID
start_gui source
source_wrapper=$STARTED_PID
wait_until "destination Rio process" 20 "rio_pid $destination_wrapper >/dev/null"
wait_until "source Rio process" 20 "rio_pid $source_wrapper >/dev/null"
destination_pid=$(rio_pid "$destination_wrapper")
source_pid=$(rio_pid "$source_wrapper")
destination_log="$CONFIG_DIR/destination/log/rio.log"
source_log="$CONFIG_DIR/source/log/rio.log"
wait_until "destination native Wayland direct startup" 30 \
    "[[ -f \"$destination_log\" ]] && kill -0 $destination_pid 2>/dev/null"
wait_until "source native Wayland direct startup" 30 \
    "[[ -f \"$source_log\" ]] && kill -0 $source_pid 2>/dev/null"
log "native Wayland Rio processes ready source=$source_pid destination=$destination_pid"
destination_ack="$RUN_DIR/destination-ack.txt"
source_ack="$RUN_DIR/source-ack.txt"
wait_until "source shell ACK" 15 "[[ -f \"$source_ack\" ]] && grep -Fq READY \"$source_ack\""
wait_until "destination shell ACK" 15 "[[ -f \"$destination_ack\" ]] && grep -Fq READY \"$destination_ack\""
source_shell_pid=$(awk -F'[ =]' '/^READY / {print $3; exit}' "$source_ack")
destination_shell_pid=$(awk -F'[ =]' '/^READY / {print $3; exit}' "$destination_ack")
sleep 1
place_script="$RUN_DIR/place-windows.js"
printf '%s\n' \
    'var windows = workspace.windowList ? workspace.windowList() : workspace.clientList();' \
    'for (var i = 0; i < windows.length; i++) {' \
    '    var window = windows[i];' \
    '    var geometry = window.frameGeometry;' \
    '    if (i === 0) { geometry.x = 0; geometry.y = 100; geometry.width = 800; geometry.height = 490; window.frameGeometry = geometry; }' \
    '    if (i === 1) { geometry.x = 800; geometry.y = 100; geometry.width = 800; geometry.height = 490; window.frameGeometry = geometry; }' \
    '}' >"$place_script"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    loadScript "$place_script" >>"$LOG_DIR/kwin-script.log" 2>&1 \
    || die "KWin window-placement script failed"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    start >>"$LOG_DIR/kwin-script.log" 2>&1 \
    || die "KWin window-placement start failed"
sleep 1
# KWin's X11 backend exposes native Wayland clients through the host surface.
# Use the same private decoration drags as the DnD harness to separate the two
# surfaces before scanning for target-local pointer events.
"$XDOTTOOL" mousemove --sync 800 252
"$XDOTTOOL" mousedown 1
"$XDOTTOOL" mousemove --sync 600 220
"$XDOTTOOL" mousemove --sync 400 180
"$XDOTTOOL" mousemove --sync 200 140
"$XDOTTOOL" mouseup 1
"$XDOTTOOL" mousemove --sync 1000 276
"$XDOTTOOL" mousedown 1
"$XDOTTOOL" mousemove --sync 1200 220
"$XDOTTOOL" mousemove --sync 1368 113
"$XDOTTOOL" mouseup 1
activate_source_script="$RUN_DIR/activate-source.js"
printf '%s\n' \
    'var windows = workspace.windowList ? workspace.windowList() : workspace.clientList();' \
    'for (var i = 0; i < windows.length; i++) {' \
    '    if (windows[i].pid === SOURCE_PID) {' \
    '        workspace.activeWindow = windows[i];' \
    '    }' \
    '}' | sed "s/SOURCE_PID/$source_pid/" >"$activate_source_script"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    loadScript "$activate_source_script" >>"$LOG_DIR/kwin-script.log" 2>&1 || die "KWin source activation script failed"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    start >>"$LOG_DIR/kwin-script.log" 2>&1 || die "KWin source activation start failed"
sleep 0.5
"$XDOTTOOL" windowactivate --sync "$host_window" 2>/dev/null || true
"$XDOTTOOL" windowfocus --sync "$host_window" 2>/dev/null || true
"$XDOTTOOL" mousemove --sync 400 148
"$XDOTTOOL" click --clearmodifiers 1
# The saved native-DnD probe used global XTest events directly. Do not require
# an X11 WM active-window or KWin grab state; native Wayland surfaces are not
# represented by the host's X11 client list.
sleep 0.5

# Native Wayland surfaces are not X11 windows, so first locate the source
# surface by moving the private host pointer and dispatching the public palette
# action at each candidate location. Capture before the one successful action;
# the later target image must therefore show the authenticated overlay rather
# than a second palette invocation. No inherited desktop coordinates are used.
capture_root "$RUN_DIR/target-unarmed.png"
try_palette() {
    local target
    local -a targets=("$host_window")
    for target in "${targets[@]}"; do
        [[ "$target" =~ ^[0-9]+$ ]] || continue
        "$XDOTTOOL" key --window "$target" --clearmodifiers ctrl+shift+p
        sleep 0.15
        "$XDOTTOOL" type --window "$target" --clearmodifiers --delay 1 "Merge Tab"
        "$XDOTTOOL" key --window "$target" --clearmodifiers Return
        sleep 0.35
        if grep -Eq "merge targets discovered|arming native merge targets" "$source_log"; then
            return 0
        fi
        if grep -Eq "merge targets discovered|arming native merge targets" "$destination_log"; then
            "$XDOTTOOL" key --window "$target" --clearmodifiers Escape
            sleep 0.1
        fi
    done
    return 1
}
source_action=false
for y in 90 210 330 450 570 690 810 930; do
    for x in 90 270 450 630 810 990 1170 1290 1410 1530; do
        "$XDOTTOOL" mousemove --sync "$x" "$y"
        "$XDOTTOOL" click --clearmodifiers 1
        if try_palette; then
            source_action=true
            break 2
        fi
    done
done
[[ "$source_action" == true ]] || die "host pointer scan could not focus the native Wayland source"
log "native Wayland source focused and keyboard palette input observed through the private host"

wait_for_log "native Wayland target arm" "$destination_log" "native merge target armed"
capture_root "$RUN_DIR/target-highlight.png"
image_changed "$RUN_DIR/target-unarmed.png" "$RUN_DIR/target-highlight.png" || \
    die "native Wayland target highlight did not change the private host image"
log "native Wayland authenticated target highlight observed"

clicked=false
target_x=
target_y=
for y in 30 90 150 210 270 330 390 450 510 570 630 690 750 810 870 930; do
    for x in 30 90 150 210 270 330 390 405 410 420 430 450 510 570 630 690 750 810 870 930 990 1050 1110 1170 1230 1290 1350 1410 1470 1530 1590; do
        "$XDOTTOOL" mousemove --sync "$x" "$y"
        sleep 0.08
        if grep -Fq "native merge target pointer" "$destination_log"; then
            "$XDOTTOOL" click --clearmodifiers 1
            target_x=$x
            target_y=$y
            clicked=true
            break 2
        fi
    done
done
[[ "$clicked" == true ]] || die "host pointer scan did not enter the native Wayland target"
wait_for_log "native Wayland target click" "$destination_log" "native merge target clicked"
wait_until "source GUI exit after native Wayland click" 20 "! kill -0 $source_pid 2>/dev/null"
log "native Wayland host pointer click committed the authenticated merge"
activate_script="$RUN_DIR/activate-destination.js"
printf '%s\n' \
    'var windows = workspace.windowList ? workspace.windowList() : workspace.clientList();' \
    'for (var i = 0; i < windows.length; i++) {' \
    '    if (windows[i].pid === DESTINATION_PID) {' \
    '        workspace.activeWindow = windows[i];' \
    '    }' \
    '}' | sed "s/DESTINATION_PID/$destination_pid/" >"$activate_script"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    loadScript "$activate_script" >>"$RUN_DIR/kwin-script.log" 2>&1 || die "KWin destination activation script failed"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    start >>"$RUN_DIR/kwin-script.log" 2>&1 || die "KWin destination activation start failed"
sleep 0.5
capture_root "$RUN_DIR/after-merge.png"
printf 'after-pointer\n' | XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$SOCKET" \
    "$WL_COPY" --primary --foreground >"$LOG_DIR/clipboard.log" 2>&1 &
CLIPBOARD_PID=$!
PIDS+=("$CLIPBOARD_PID")
wait_until "private Wayland primary selection" 5 "kill -0 $CLIPBOARD_PID 2>/dev/null"
"$XDOTTOOL" key --window "$host_window" --clearmodifiers super+1
sleep 0.2
paste_ack=false
for y in "$target_y" 180 250 320 420 520; do
    for x in "$target_x" 900 1100 1300 1500; do
        [[ "$x" =~ ^[0-9]+$ && "$y" =~ ^[0-9]+$ ]] || continue
        "$XDOTTOOL" mousemove --sync "$x" "$y"
        "$XDOTTOOL" click --clearmodifiers 2
        sleep 0.35
        if grep -Fq "ACK pid=$source_shell_pid input=after-pointer" "$source_ack"; then
            paste_ack=true
            break 2
        fi
    done
done
[[ "$paste_ack" == true ]] || die "native Wayland transferred shell did not receive a fresh primary-selection paste ACK"
kill "$CLIPBOARD_PID" 2>/dev/null || true
wait "$CLIPBOARD_PID" 2>/dev/null || true
CLIPBOARD_PID=
log "native Wayland transferred shell retained a fresh middle-click primary-selection ACK after the direct-render merge"

{
    printf '\n[%s] process snapshot (private X11-backed Wayland pointer)\n' "$(date -u +%H:%M:%S)"
    printf 'pid ppid cpu_percent rss_kib command\n'
    for root in "$kwin_pid" "$destination_pid"; do
        printf '%s\n' "$root"
        owned_descendants "$root"
    done | while read -r pid; do
        [[ "$pid" =~ ^[0-9]+$ ]] || continue
        ps -p "$pid" -o pid=,ppid=,%cpu=,rss=,args= 2>/dev/null || true
    done
} | tee -a "$RUN_DIR/summary.log"
log "PASS: private nested Wayland host-pointer, keyboard-palette, and primary-selection acceptance completed"
log "UNVERIFIED: native Wayland DnD payload transfer remains outside this pointer-focused harness"
