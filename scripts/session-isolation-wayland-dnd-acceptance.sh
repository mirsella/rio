#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# Validate the current direct-render build's native Wayland drag path.  KWin's
# X11 backend is used only inside the private Xvfb server so XTest events never
# reach the user's desktop.  The Rio clients have DISPLAY unset and therefore
# use only the private Wayland socket.

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARTIFACT_ROOT=${RIO_ACCEPT_ARTIFACT_ROOT:-"$HOME/dev/rio-agent-artifacts"}
RUN_DIR="$ARTIFACT_ROOT/session-isolation-wayland-dnd-$(date -u +%Y%m%dT%H%M%SZ)-$$"
CONFIG_DIR="$RUN_DIR/config"
LOG_DIR="$RUN_DIR/logs"
RUNTIME_DIR="$ARTIFACT_ROOT/r-$$"
TMP_DIR="$ARTIFACT_ROOT/t-$$"
BUS_ADDRESS="unix:path=$RUNTIME_DIR/bus"

RIO_BIN=${RIO_BIN:-"$ROOT/target/debug/rio"}
XVFB=${XVFB:-/usr/bin/Xvfb}
KWIN=${KWIN:-/usr/bin/kwin_wayland}
DBUS_DAEMON=${DBUS_DAEMON:-/usr/bin/dbus-daemon}
XDPYINFO=${XDPYINFO:-/usr/bin/xdpyinfo}
XDOTTOOL=${XDOTTOOL:-/usr/bin/xdotool}
SETXKBMAP=${SETXKBMAP:-/usr/bin/setxkbmap}
VULKANINFO=${VULKANINFO:-/usr/bin/vulkaninfo}
IMPORT=${IMPORT:-/usr/bin/import}
QDBUS6=${QDBUS6:-/usr/bin/qdbus6}
WAYLAND_INFO=${WAYLAND_INFO:-/usr/bin/wayland-info}
WL_COPY=${WL_COPY:-/usr/bin/wl-copy}
SHELL_FIXTURE="$ROOT/scripts/fixtures/session-isolation-ack-shell.sh"
SOCKET=${RIO_ACCEPT_WAYLAND_SOCKET:-rio-direct-dnd}
DISPLAY_NUM=${RIO_ACCEPT_X_DISPLAY:-$((700 + ($$ % 100)))}
export DISPLAY=":$DISPLAY_NUM"
export XDG_RUNTIME_DIR="$RUNTIME_DIR"
export TMPDIR="$TMP_DIR"

mkdir -p "$CONFIG_DIR/source/log" "$CONFIG_DIR/destination/log" "$LOG_DIR" \
    "$RUNTIME_DIR" "$TMP_DIR" "$RUN_DIR/home" "$RUN_DIR/kwin-config" \
    "$RUN_DIR/kwin-data" "$RUN_DIR/kwin-cache"
chmod 700 "$RUN_DIR" "$CONFIG_DIR" "$RUNTIME_DIR" "$TMP_DIR"
unset WAYLAND_DISPLAY WAYLAND_SOCKET

PIDS=()
ORPHANS=()
XVFB_PID=
DBUS_PID=
CLIPBOARD_PID=

trap cleanup EXIT INT TERM

rio_descendant() {
    local root=$1 pid args
    for pid in "$root" $(owned_descendants "$root"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        if [[ "$args" == *"$RIO_BIN"* && "$args" != *"--endpoint"* ]]; then
            printf '%s\n' "$pid"
            return 0
        fi
    done
    return 1
}

session_worker() {
    local root=$1 pid args
    for pid in "$root" $(owned_descendants "$root"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        if [[ "$args" == *"--endpoint"* ]]; then
            printf '%s\n' "$pid"
            return 0
        fi
    done
    return 1
}

shell_child() {
    local worker=$1 pid args
    for pid in $(owned_descendants "$worker"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        [[ "$args" == *"bash -lc"* || "$args" == *"$SHELL_FIXTURE"* ]] && {
            printf '%s\n' "$pid"
            return 0
        }
    done
    return 1
}

log_tree() {
    local root=$1 pid
    for pid in "$root" $(owned_descendants "$root"); do
        ps -p "$pid" -o pid=,ppid=,args= >>"$RUN_DIR/process-tree.log" 2>/dev/null || true
    done
}

wait_for_optional_text() {
    local file=$1 text=$2 deadline=$((SECONDS + 10))
    while ((SECONDS < deadline)); do
        if [[ -f "$file" ]] && grep -Fq -- "$text" "$file"; then
            return 0
        fi
        sleep 0.1
    done
    return 1
}

start_gui() {
    local role=$1 ack_file="$RUN_DIR/$1-ack.txt" config="$CONFIG_DIR/$1"
    XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$SOCKET" \
        DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" XDG_SESSION_TYPE=wayland \
        RIO_CONFIG_HOME="$config" RIO_LOG_LEVEL=INFO RIO_ACCEPT_ACK_FILE="$ack_file" \
        nohup setsid env -u DISPLAY "$RIO_BIN" --enable-log-file \
        --title-placeholder "rio-direct-dnd-$role" --app-id "rio-direct-dnd-$role" \
        -e /bin/sh "$SHELL_FIXTURE" </dev/null >"$LOG_DIR/$role.stdout" 2>&1 &
    STARTED_PID=$!
    PIDS+=("$STARTED_PID")
}

for command in "$XVFB" "$KWIN" "$DBUS_DAEMON" "$XDPYINFO" "$XDOTTOOL" \
    "$SETXKBMAP" "$VULKANINFO" "$IMPORT" "$QDBUS6" "$WAYLAND_INFO" \
    "$WL_COPY" pgrep ps awk grep; do
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
log "input=private XTest pointer plus private Wayland primary selection"

"$XVFB" "$DISPLAY" -screen 0 1600x1000x24 -nolisten tcp -ac >"$LOG_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
wait_until "private Xvfb" 15 "\"$XDPYINFO\" -display \"$DISPLAY\" >/dev/null 2>&1"
DISPLAY="$DISPLAY" "$SETXKBMAP" -layout us -variant colemak_dh_iso

DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$DBUS_DAEMON" --session \
    --address="$BUS_ADDRESS" --nofork >"$LOG_DIR/dbus.log" 2>&1 &
DBUS_PID=$!
wait_until "private D-Bus" 15 "kill -0 $DBUS_PID 2>/dev/null"

HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/kwin-config" \
    XDG_DATA_HOME="$RUN_DIR/kwin-data" XDG_CACHE_HOME="$RUN_DIR/kwin-cache" \
    DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" QT_QPA_PLATFORM=xcb \
    LIBGL_ALWAYS_SOFTWARE=1 "$KWIN" --socket="$SOCKET" \
    --x11-display="$DISPLAY" --width=1600 --height=1000 --no-lockscreen \
    --no-global-shortcuts --no-kactivities >"$LOG_DIR/kwin.log" 2>&1 &
kwin_pid=$!
PIDS+=("$kwin_pid")
wait_until "private KWin Wayland socket" 15 \
    "[[ -S \"$RUNTIME_DIR/$SOCKET\" ]] && kill -0 $kwin_pid 2>/dev/null"
XDG_RUNTIME_DIR="$RUNTIME_DIR" "$VULKANINFO" --summary >"$LOG_DIR/vulkaninfo.log" 2>&1 || true
XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$SOCKET" "$WAYLAND_INFO" \
    >"$LOG_DIR/wayland-info.log" 2>&1 || die "private Wayland global enumeration failed"
if grep -Eiq 'virtual.?keyboard' "$LOG_DIR/wayland-info.log"; then
    log "private Wayland virtual-keyboard global advertised"
else
    log "UNSUPPORTED: private KWin compositor advertises no virtual-keyboard global"
fi
host_window=$(DISPLAY="$DISPLAY" "$XDOTTOOL" getactivewindow 2>/dev/null || true)
if [[ ! "$host_window" =~ ^[0-9]+$ ]]; then
    host_window=$(DISPLAY="$DISPLAY" "$XDOTTOOL" search --all --name '' 2>/dev/null | head -n 1 || true)
fi
[[ "$host_window" =~ ^[0-9]+$ ]] || die "private KWin host window not found"

for role in source destination; do
    printf '%s\n' \
        'draw-bold-text-with-light-colors = true' '' '[renderer]' 'use-cpu = true' '' \
        '[developer]' 'log-level = "INFO"' >"$CONFIG_DIR/$role/config.toml"
done

start_gui source
source_wrapper=$STARTED_PID
source_ack="$RUN_DIR/source-ack.txt"
wait_for_text "$CONFIG_DIR/source/log/rio.log" 'Initialisation complete' "source Rio client"
sleep 1
log_tree "$source_wrapper"
wait_for_text "$source_ack" READY "source shell"
source_gui=$(rio_descendant "$source_wrapper") || die "source Rio process not found"
source_worker=$(session_worker "$source_gui") || {
    log_tree "$source_wrapper"
    ps -p "$kwin_pid" -o pid=,ppid=,stat=,args= >>"$RUN_DIR/process-tree.log" 2>/dev/null || true
    die "source session worker not found"
}
source_shell=$(shell_child "$source_worker") || {
    log_tree "$source_wrapper"
    die "source shell not found"
}

start_gui destination
destination_wrapper=$STARTED_PID
wait_for_text "$CONFIG_DIR/destination/log/rio.log" 'Initialisation complete' "destination Rio client"
destination_gui=$(rio_descendant "$destination_wrapper") || die "destination Rio process not found"
destination_worker=$(session_worker "$destination_gui") || {
    log_tree "$destination_wrapper"
    die "destination session worker not found"
}
destination_shell=$(shell_child "$destination_worker") || die "destination shell not found"
[[ "$source_shell" =~ ^[0-9]+$ && "$destination_shell" =~ ^[0-9]+$ ]] || die "shell PIDs not found"
ORPHANS+=("$source_worker" "$source_shell" "$destination_worker" "$destination_shell")
destination_ack="$RUN_DIR/destination-ack.txt"
wait_for_text "$destination_ack" READY "destination shell"
log "source worker=$source_worker shell=$source_shell"
log "destination worker=$destination_worker shell=$destination_shell"
sleep 1

kwin_script="$RUN_DIR/place-windows.js"
printf '%s\n' \
    'var windows = workspace.windowList ? workspace.windowList() : workspace.clientList();' \
    'for (var i = 0; i < windows.length; i++) {' \
    '    var window = windows[i];' \
    '    var geometry = window.frameGeometry;' \
    '    if (i === 0) { geometry.x = 0; geometry.y = 100; geometry.width = 800; geometry.height = 490; window.frameGeometry = geometry; }' \
    '    if (i === 1) { geometry.x = 800; geometry.y = 100; geometry.width = 800; geometry.height = 490; window.frameGeometry = geometry; }' \
    '}' >"$kwin_script"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    loadScript "$kwin_script" >>"$RUN_DIR/kwin-script.log" 2>&1 || die "KWin window-placement script failed"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    start >>"$RUN_DIR/kwin-script.log" 2>&1 || die "KWin window-placement start failed"
sleep 1

# KWin's X11 backend exposes native Wayland clients only through its host
# surface. Move the two private decorations apart with the same XTest stream
# used for the drag, avoiding desktop-wide window IDs or a host WM dependency.
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

DISPLAY="$DISPLAY" "$IMPORT" -window root "$RUN_DIR/before-dnd.png"

# The saved successful native-DnD probe used this XTest path.  It crosses the
# tab strip from the source surface into the destination surface without using
# a desktop-wide injector or any production-only testing hook.
before_source=$(cat "$source_ack")
"$XDOTTOOL" mousemove --sync 200 176
"$XDOTTOOL" mousedown 1
for point in '300 176' '500 160' '700 150' '900 148' '1200 148'; do
    read -r x y <<<"$point"
    "$XDOTTOOL" mousemove --sync "$x" "$y"
done
"$XDOTTOOL" mouseup 1

wait_for_text "$CONFIG_DIR/destination/log/rio.log" 'event="data-ready"' \
    "native Wayland target data delivery"
wait_for_text "$CONFIG_DIR/destination/log/rio.log" 'ready_routes=[2]' \
    "direct-render import readiness"
wait_until "source GUI exit after native DnD" 30 "! kill -0 $source_wrapper 2>/dev/null"
kill -0 "$source_worker" 2>/dev/null || die "source worker did not survive source GUI exit"
kill -0 "$source_shell" 2>/dev/null || die "source shell did not survive source GUI exit"
log "source GUI exited; worker and shell survived authenticated commit"

DISPLAY="$DISPLAY" "$IMPORT" -window root "$RUN_DIR/after-dnd.png"
wait_until "destination GUI remains alive" 10 "kill -0 $destination_wrapper 2>/dev/null"
activate_script="$RUN_DIR/activate-destination.js"
printf '%s\n' \
    'var windows = workspace.windowList ? workspace.windowList() : workspace.clientList();' \
    'for (var i = 0; i < windows.length; i++) {' \
    '    if (windows[i].pid === DESTINATION_PID) {' \
    '        workspace.activeWindow = windows[i];' \
    '    }' \
    '}' | sed "s/DESTINATION_PID/$destination_gui/" >"$activate_script"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    loadScript "$activate_script" >>"$RUN_DIR/kwin-script.log" 2>&1 || die "KWin destination activation script failed"
DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
    start >>"$RUN_DIR/kwin-script.log" 2>&1 || die "KWin destination activation start failed"
sleep 0.5
printf 'after-dnd\n' | XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$SOCKET" \
    "$WL_COPY" --primary --foreground >"$LOG_DIR/clipboard.log" 2>&1 &
CLIPBOARD_PID=$!
PIDS+=("$CLIPBOARD_PID")
wait_until "private Wayland primary selection" 5 "kill -0 $CLIPBOARD_PID 2>/dev/null"
paste_ack=false
paste_x=
paste_y=
# The transferred shell is the only process that can create this ACK. Use the
# same bounded private destination rectangle as the DnD path, but exercise the
# native pointer binding (middle-click paste) rather than guessing an X11
# keyboard focus for a Wayland surface.
for y in 180 250 320 420 520; do
    for x in 900 1100 1300 1500; do
        "$XDOTTOOL" mousemove --sync "$x" "$y"
        "$XDOTTOOL" click --clearmodifiers 2
        sleep 0.35
        if grep -Fq "ACK pid=$source_shell input=after-dnd" "$source_ack"; then
            paste_ack=true
            paste_x=$x
            paste_y=$y
            break 2
        fi
    done
done
[[ "$paste_ack" == true ]] || die "private native Wayland primary-selection paste did not produce a transferred-shell ACK"
kill "$CLIPBOARD_PID" 2>/dev/null || true
wait "$CLIPBOARD_PID" 2>/dev/null || true
CLIPBOARD_PID=
log "post-DnD native middle-click paste produced a fresh ACK from the transferred shell"

native_key=false
"$XDOTTOOL" windowactivate --sync "$host_window" 2>>"$LOG_DIR/keyboard.log" || true
"$XDOTTOOL" mousemove --sync "$paste_x" "$paste_y"
"$XDOTTOOL" click --clearmodifiers 1
sleep 0.2
"$XDOTTOOL" type --window "$host_window" --clearmodifiers --delay 10 after-key
"$XDOTTOOL" key --window "$host_window" --clearmodifiers Return
sleep 0.5
if grep -Fq "ACK pid=$source_shell input=after-key" "$source_ack"; then
    native_key=true
    log "post-DnD private XTest keyboard input produced a fresh ACK from the transferred shell"
else
    log "UNSUPPORTED: private XTest keyboard input did not reach the native Wayland surface"
fi
printf 'source_before=%s\n' "$before_source" >"$RUN_DIR/evidence.txt"
grep -F "READY pid=$source_shell" "$source_ack" >"$RUN_DIR/evidence.txt.tmp" \
    || die "transferred shell readiness marker was not retained"
printf 'transferred_shell=%s\n' "$(cat "$RUN_DIR/evidence.txt.tmp")" >>"$RUN_DIR/evidence.txt"
rm -f "$RUN_DIR/evidence.txt.tmp"
printf 'source_worker=%s\nsource_shell=%s\ndestination_worker=%s\ndestination_shell=%s\n' \
    "$source_worker" "$source_shell" "$destination_worker" "$destination_shell" >>"$RUN_DIR/evidence.txt"
printf 'native_middle_click_paste=PASS\nnative_key_input=%s\n' \
    "$([[ "$native_key" == true ]] && printf PASS || printf UNSUPPORTED)" >>"$RUN_DIR/evidence.txt"
log "PASS: current direct-render native Wayland DnD authenticated commit and shell continuity completed"
log "native Wayland post-DnD middle-click input is covered; key input result is recorded separately"
