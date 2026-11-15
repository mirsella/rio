# Shared lifecycle helpers for the private acceptance harnesses.
# This file is sourced by scripts in the parent directory.

# Qt sends logs to journald when stderr is not a terminal; keep the private
# KWin output in each run's kwin.log instead.
export QT_FORCE_STDERR_LOGGING=1

log() {
    if [[ -n ${SI_LOG_PREFIX:-} ]]; then
        printf '%s%s\n' "$SI_LOG_PREFIX" "$*" | tee -a "$RUN_DIR/summary.log"
    else
        printf '[%s] %s\n' "$(date -u +%H:%M:%S)" "$*" | tee -a "$RUN_DIR/summary.log"
    fi
}

die() {
    log "FAIL: $*"
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command is missing: $1"
}

owned_descendants() {
    local parent=$1 child
    while read -r child; do
        [[ -n "$child" ]] || continue
        printf '%s\n' "$child"
        owned_descendants "$child"
    done < <(pgrep -P "$parent" 2>/dev/null || true)
}

window_pid() {
    "${XPROP:-xprop}" -id "$1" _NET_WM_PID 2>/dev/null |
        awk -F' = ' 'NF == 2 {print $2; exit}'
}

window_for_pid() {
    local root=$1 window pid
    while read -r window; do
        [[ -n "$window" ]] || continue
        pid=$(window_pid "$window" || true)
        if [[ "$pid" == "$root" ]]; then
            printf '%s\n' "$window"
            return 0
        fi
    done < <("${XDOTTOOL:-xdotool}" search --onlyvisible --name '.*' 2>/dev/null || true)
    return 1
}

stop_tree() {
    local root pid index
    local -a tree=()
    local -A seen=()
    # Capture every tree before stopping a display can orphan its Rio workers.
    for root in "$@"; do
        [[ "$root" =~ ^[1-9][0-9]*$ ]] || continue
        while read -r pid; do
            [[ -n "$pid" && ! ${seen[$pid]+present} ]] || continue
            seen[$pid]=1
            tree+=("$pid")
        done < <(printf '%s\n' "$root"; owned_descendants "$root")
    done
    for ((index = ${#tree[@]} - 1; index >= 0; index--)); do
        kill -TERM "${tree[index]}" 2>/dev/null || true
    done
    sleep "${SI_STOP_GRACE_SECONDS:-0.3}"
    for ((index = ${#tree[@]} - 1; index >= 0; index--)); do
        pid=${tree[index]}
        kill -KILL "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    done
}

cleanup() {
    trap - EXIT INT TERM
    local -a roots=()
    [[ ${PIDS+x} ]] && roots+=("${PIDS[@]}")
    [[ ${ORPHANS+x} ]] && roots+=("${ORPHANS[@]}")
    [[ ${XVFB_PID+x} ]] && roots+=("$XVFB_PID")
    [[ ${OPENBOX_PID+x} ]] && roots+=("$OPENBOX_PID")
    [[ ${DBUS_PID+x} ]] && roots+=("$DBUS_PID")
    [[ ${CLIPBOARD_PID+x} ]] && roots+=("$CLIPBOARD_PID")
    roots+=("$@")
    stop_tree "${roots[@]}"
}

wait_until() {
    local description=$1 timeout=$2 check=$3
    local deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        if eval "$check"; then
            return 0
        fi
        sleep "${SI_WAIT_POLL_SECONDS:-${ACK_POLL_SECONDS:-0.1}}"
    done
    die "timed out waiting for $description"
}

wait_for_text() {
    local file=$1 text=$2 description=${3:-"'$text' in $file"} timeout=${4:-30}
    local file_q text_q
    printf -v file_q '%q' "$file"
    printf -v text_q '%q' "$text"
    wait_until "$description" "$timeout" \
        "[[ -f $file_q ]] && grep -Fq -- $text_q $file_q"
}

# Start the private X server that hosts KWin's X11 backend. KWin 6 needs DRI3
# there, which Xvfb lacks (KWin exits silently right after connecting), so by
# default the X server is a rootful Xwayland on its own private headless KWin.
# RIO_ACCEPT_X11_HOST=xvfb keeps the plain Xvfb host. Sets XVFB_PID to the X
# server; the host compositor is added to PIDS. Needs XDG_RUNTIME_DIR.
write_us_kxkbrc() {
    mkdir -p "$1"
    printf '[Layout]\nLayoutList=us\nUse=true\n' >"$1/kxkbrc"
}

# xdotool turns keysyms into keycodes with the X server's keymap, and the
# nested KWin decodes them with its own; both must be plain US. Xwayland takes
# its keymap from the host KWin, and setxkbmap on a live Xwayland kills other
# X clients, so only check the result here.
pin_private_keymap() {
    local display=$1 kwin_config=$2 query
    write_us_kxkbrc "$kwin_config"
    query=$(DISPLAY="$display" "${SETXKBMAP:-/usr/bin/setxkbmap}" -query 2>/dev/null) ||
        die "could not query the private X keymap"
    grep -qxE 'layout:[[:space:]]+us' <<<"$query" && ! grep -q '^variant:' <<<"$query" ||
        die "private X keymap is not plain US: ${query//$'\n'/ }"
}

start_private_x11_host() {
    local display=$1 width=$2 height=$3 log_dir=$4
    local xdpyinfo=${XDPYINFO:-/usr/bin/xdpyinfo}
    if [[ ${RIO_ACCEPT_X11_HOST:-xwayland} == xvfb ]]; then
        "${XVFB:-/usr/bin/Xvfb}" "$display" -screen 0 "${width}x${height}x24" \
            -nolisten tcp -ac >"$log_dir/xvfb.log" 2>&1 &
        XVFB_PID=$!
    else
        local kwin=${KWIN:-/usr/bin/kwin_wayland}
        local xwayland=${XWAYLAND:-/usr/bin/Xwayland}
        local dbus_run_session=${DBUS_RUN_SESSION:-/usr/bin/dbus-run-session}
        local socket="rio-x11-host-$$" host_pid
        require_command "$xwayland"
        require_command "$dbus_run_session"
        write_us_kxkbrc "$log_dir/x11-host-config"
        # dbus-run-session gives the host compositor its own bus, never the
        # user's session bus.
        env -u DISPLAY -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
            XDG_CONFIG_HOME="$log_dir/x11-host-config" \
            "$dbus_run_session" -- "$kwin" --virtual --socket="$socket" \
            --width="$width" --height="$height" --no-lockscreen \
            --no-global-shortcuts --no-kactivities \
            >"$log_dir/x11-host-kwin.log" 2>&1 &
        host_pid=$!
        PIDS+=("$host_pid")
        wait_until "private X11 host compositor" 15 \
            "[[ -S \"$XDG_RUNTIME_DIR/$socket\" ]] && kill -0 $host_pid 2>/dev/null"
        env -u DISPLAY WAYLAND_DISPLAY="$socket" "$xwayland" "$display" \
            -geometry "${width}x${height}" -nolisten tcp -ac \
            >"$log_dir/xwayland.log" 2>&1 &
        XVFB_PID=$!
    fi
    wait_until "private X server" 15 \
        "kill -0 $XVFB_PID 2>/dev/null && \"$xdpyinfo\" -display \"$display\" >/dev/null 2>&1"
}
