#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# This harness owns a nested KWin compositor and never touches the inherited
# desktop. It validates the live direct Sugarloaf GPU window path. Set
# RIO_ACCEPT_KWIN_X11=1 to run KWin on a private Xvfb host.

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARTIFACT_ROOT=${RIO_ACCEPT_ARTIFACT_ROOT:-"$HOME/dev/rio-agent-artifacts"}
RUN_ID="session-isolation-wayland-$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_DIR="$ARTIFACT_ROOT/$RUN_ID"
LOG_DIR="$RUN_DIR/logs"
CONFIG_DIR="$RUN_DIR/config"
# Keep the runtime and temporary paths short enough for Unix-domain sockets.
RUNTIME_DIR="$ARTIFACT_ROOT/w-$$"
TMP_DIR="$ARTIFACT_ROOT/t-$$"

RIO_BIN=${RIO_BIN:-"$ROOT/target/debug/rio"}
KWIN=${KWIN:-/usr/bin/kwin_wayland}
DBUS_RUN_SESSION=${DBUS_RUN_SESSION:-/usr/bin/dbus-run-session}
RIO_ACCEPT_WAYLAND_SOCKET=${RIO_ACCEPT_WAYLAND_SOCKET:-rio-accept-wayland}
XVFB=${XVFB:-/usr/bin/Xvfb}
XDPYINFO=${XDPYINFO:-/usr/bin/xdpyinfo}
XDOTTOOL=${XDOTTOOL:-/usr/bin/xdotool}
XPROP=${XPROP:-/usr/bin/xprop}
IMPORT=${IMPORT:-/usr/bin/import}
COMPARE=${COMPARE:-/usr/bin/compare}
USE_PRIVATE_X11=${RIO_ACCEPT_KWIN_X11:-0}

mkdir -p "$LOG_DIR" "$CONFIG_DIR/kwin" "$CONFIG_DIR/rio" "$RUNTIME_DIR" "$TMP_DIR"
chmod 700 "$RUN_DIR" "$CONFIG_DIR" "$RUNTIME_DIR" "$TMP_DIR"

rio_use_cpu=false

export TMPDIR="$TMP_DIR"
export XDG_RUNTIME_DIR="$RUNTIME_DIR"
export XDG_CONFIG_HOME="$CONFIG_DIR/kwin"
unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET

PIDS=()

trap cleanup EXIT INT TERM

if [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == TRUE || "$USE_PRIVATE_X11" == yes ]]; then
    rio_use_cpu=true
    for command in "$XVFB" "$XDPYINFO" "$XDOTTOOL" "$XPROP" "$IMPORT" "$COMPARE"; do
        require_command "$command"
    done
    DISPLAY_NUM=${RIO_ACCEPT_X_DISPLAY:-$((200 + ($$ % 400)))}
    export DISPLAY=":$DISPLAY_NUM"
    "$XVFB" "$DISPLAY" -screen 0 1280x720x24 -nolisten tcp -ac >"$LOG_DIR/xvfb.log" 2>&1 &
    xvfb_pid=$!
    PIDS+=("$xvfb_pid")
    wait_until "private Xvfb display" 15 "\"$XDPYINFO\" -display \"$DISPLAY\" >/dev/null 2>&1"
    log "private Xvfb host ready display=$DISPLAY pid=$xvfb_pid"
fi

for command in "$KWIN" "$DBUS_RUN_SESSION" pgrep ps grep; do
    require_command "$command"
done
[[ -x "$RIO_BIN" ]] || die "RIO_BIN is not executable: $RIO_BIN"

log "run_dir=$RUN_DIR"
log "binary=$RIO_BIN"
log "runtime=$XDG_RUNTIME_DIR"
log "tmpdir=$TMPDIR"
log "wayland_socket=$RIO_ACCEPT_WAYLAND_SOCKET"
log "kwin_host=${USE_PRIVATE_X11:-0}"
printf '%s\n' \
    'draw-bold-text-with-light-colors = true' \
    '' \
    '[renderer]' \
    "use-cpu = $rio_use_cpu" \
    '' \
    '[developer]' \
    'log-level = "INFO"' \
    >"$CONFIG_DIR/rio/config.toml"

kwin_args=(--socket="$RIO_ACCEPT_WAYLAND_SOCKET" --xwayland --no-global-shortcuts --no-kactivities)
if [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == TRUE || "$USE_PRIVATE_X11" == yes ]]; then
    kwin_args+=(--x11-display="$DISPLAY")
else
    kwin_args+=(--virtual)
fi
XDG_RUNTIME_DIR="$RUNTIME_DIR" XDG_CONFIG_HOME="$CONFIG_DIR/kwin" \
    "$DBUS_RUN_SESSION" -- "$KWIN" "${kwin_args[@]}" \
    >"$LOG_DIR/kwin.log" 2>&1 &
kwin_wrapper_pid=$!
PIDS+=("$kwin_wrapper_pid")
wait_until "nested KWin Wayland socket" 15 \
    "[[ -S \"$RUNTIME_DIR/$RIO_ACCEPT_WAYLAND_SOCKET\" ]] && kill -0 $kwin_wrapper_pid 2>/dev/null"
log "nested KWin Wayland compositor ready pid=$kwin_wrapper_pid"

XDG_RUNTIME_DIR="$RUNTIME_DIR" RIO_CONFIG_HOME="$CONFIG_DIR/rio" TMPDIR="$TMP_DIR" \
    WAYLAND_DISPLAY="$RIO_ACCEPT_WAYLAND_SOCKET" RIO_LOG_LEVEL=INFO \
    "$DBUS_RUN_SESSION" -- "$RIO_BIN" --enable-log-file \
    --title-placeholder rio-wayland-accept --app-id rio-wayland-accept \
    -e bash -lc 'while IFS= read -r line; do :; done' \
    >"$LOG_DIR/rio.stdout" 2>&1 &
rio_wrapper_pid=$!
PIDS+=("$rio_wrapper_pid")
rio_log="$CONFIG_DIR/rio/log/rio.log"
wait_until "Wayland Rio direct-render startup" 30 \
    "[[ -f \"$rio_log\" ]] && kill -0 $rio_wrapper_pid 2>/dev/null"
log "live Wayland frontend reached the direct Sugarloaf render path"

if [[ "$rio_use_cpu" == false ]]; then
    wait_until "Rio GPU device and surface initialization" 30 \
        "kill -0 $rio_wrapper_pid 2>/dev/null && grep -Eq 'Vulkan device created:|Selected adapter:' \"$rio_log\" && grep -Eq 'Swapchain:|Surface format:' \"$rio_log\""
    grep -E 'Vulkan device created:|Selected adapter:|Swapchain:|Surface format:' \
        "$rio_log" | tee -a "$RUN_DIR/summary.log"
    log "GPU startup verified from the tested Rio process"
fi

{
    printf '\n[%s] process snapshot (nested Wayland direct GPU)\n' "$(date -u +%H:%M:%S)"
    printf 'pid ppid cpu_percent rss_kib command\n'
    for root in "$kwin_wrapper_pid" "$rio_wrapper_pid"; do
        printf '%s\n' "$root"
        owned_descendants "$root"
    done | while read -r pid; do
        [[ "$pid" =~ ^[0-9]+$ ]] || continue
        ps -p "$pid" -o pid=,ppid=,%cpu=,rss=,args= 2>/dev/null || true
    done
} | tee -a "$RUN_DIR/summary.log"
log "PASS: private nested Wayland direct rendering startup completed (use-cpu=$rio_use_cpu)"
log "UNVERIFIED: native Wayland DnD and target-local pointer click require a native Wayland input injector not used by this harness"
