#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# Compare the live Wayland GPU path in a private KWin virtual framebuffer (or
# optional Xvfb-backed KWin). Images
# are compared only after cropping the terminal content area out of the
# private frame, so decoration and host-compositor pixels do not dominate the
# result.

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARTIFACT_ROOT=${RIO_ACCEPT_ARTIFACT_ROOT:-"$HOME/dev/rio-agent-artifacts"}
RUN_ID="session-isolation-wayland-visual-parity-$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_DIR="$ARTIFACT_ROOT/$RUN_ID"
LOG_DIR="$RUN_DIR/logs"
CONFIG_DIR="$RUN_DIR/config"
RUNTIME_DIR="$ARTIFACT_ROOT/w-$$"
TMP_DIR="$ARTIFACT_ROOT/t-$$"
BUS_ADDRESS="unix:path=$RUNTIME_DIR/bus"
DISPLAY_NUM=${RIO_ACCEPT_X_DISPLAY:-$((780 + ($$ % 80)))}
DISPLAY=":$DISPLAY_NUM"
SOCKET=${RIO_ACCEPT_WAYLAND_SOCKET:-rio-gpu-visual}

RIO_CURRENT_BIN=${RIO_CURRENT_BIN:-"$ROOT/target/debug/rio"}
RIO_UPSTREAM_BIN=${RIO_UPSTREAM_BIN:-"$ARTIFACT_ROOT/target-upstream-direct-bench/debug/rio"}
FIXTURE="$ROOT/scripts/fixtures/session-isolation-graphics-fixture.sh"
XVFB=${XVFB:-/usr/bin/Xvfb}
KWIN=${KWIN:-/usr/bin/kwin_wayland}
DBUS_DAEMON=${DBUS_DAEMON:-/usr/bin/dbus-daemon}
XDPYINFO=${XDPYINFO:-/usr/bin/xdpyinfo}
IMPORT=${IMPORT:-/usr/bin/import}
CONVERT=${CONVERT:-/usr/bin/convert}
COMPARE=${COMPARE:-/usr/bin/compare}
IDENTIFY=${IDENTIFY:-/usr/bin/identify}
QDBUS6=${QDBUS6:-/usr/bin/qdbus6}
VULKANINFO=${VULKANINFO:-/usr/bin/vulkaninfo}
VKCUBE=${VKCUBE:-/usr/bin/vkcube}
WAYLAND_INFO=${WAYLAND_INFO:-/usr/bin/wayland-info}
SPECTACLE=${SPECTACLE:-/usr/bin/spectacle}
RADEONTOP=${RADEONTOP:-/usr/bin/radeontop}
XDOTTOOL=${XDOTTOOL:-/usr/bin/xdotool}
USE_PRIVATE_X11=${RIO_ACCEPT_KWIN_X11:-0}
RIO_ACCEPT_GPU_SELECTION_ONLY=${RIO_ACCEPT_GPU_SELECTION_ONLY:-0}
VISUAL_STATES=(base palette cursor underline kitty sixel alternate)
KITTY_RED_REGION='48x48+420+283'
KITTY_GREEN_REGION='48x48+468+283'
KITTY_BLUE_REGION='48x48+420+331'
KITTY_WHITE_REGION='48x48+468+331'
SIXEL_RED_REGION='48x6+420+283'
SIXEL_BLUE_REGION='48x6+420+289'

PIDS=()
XVFB_PID=
DBUS_PID=

mkdir -p "$LOG_DIR" "$CONFIG_DIR" "$RUNTIME_DIR" "$TMP_DIR"
chmod 700 "$RUN_DIR" "$CONFIG_DIR" "$RUNTIME_DIR" "$TMP_DIR"
export DISPLAY XDG_RUNTIME_DIR="$RUNTIME_DIR" TMPDIR="$TMP_DIR"
unset WAYLAND_DISPLAY WAYLAND_SOCKET

trap cleanup EXIT INT TERM

rio_process_for() {
    local root=$1 binary=$2 pid args
    for pid in "$root" $(owned_descendants "$root"); do
        args=$(ps -p "$pid" -o args= 2>/dev/null || true)
        if [[ "$args" == *"$binary"* && "$args" != *"--endpoint"* ]]; then
            printf '%s\n' "$pid"
            return 0
        fi
    done
    return 1
}

write_config() {
    local config=$1 filter=$2 backend=${3:-}
    mkdir -p "$config/log"
    chmod 700 "$config"
    printf '%s\n' \
        'draw-bold-text-with-light-colors = true' \
        'confirm-before-quit = false' \
        '' '[renderer]' 'use-cpu = false' \
        >"$config/config.toml"
    if [[ "$filter" != none ]]; then
        printf '%s\n' 'backend = "webgpu"' "filters = [\"$filter\"]" \
            >>"$config/config.toml"
    elif [[ -n "$backend" ]]; then
        printf 'backend = "%s"\n' "$backend" >>"$config/config.toml"
    fi
    printf '%s\n' \
        '' '[bindings]' \
        'keys = [{ key = "a", with = "control|shift", action = "selectall" }]' \
        '' '[developer]' 'log-level = "INFO"' >>"$config/config.toml"
}

place_window() {
    local pid=$1 phase=$2 variant=$3 script="$RUN_DIR/place-$phase-$variant.js"
    printf '%s\n' \
        'var windows = workspace.windowList ? workspace.windowList() : workspace.clientList();' \
        'for (var i = 0; i < windows.length; i++) {' \
        "    if (windows[i].pid == $pid) {" \
        '        var geometry = windows[i].frameGeometry;' \
        '        geometry.x = 0; geometry.y = 100; geometry.width = 800; geometry.height = 490;' \
        '        windows[i].frameGeometry = geometry;' \
        '    }' \
        '}' >"$script"
    DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
        loadScript "$script" >>"$LOG_DIR/kwin-script.log" 2>&1 \
        || die "KWin window-placement script failed for $phase/$variant"
    DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$QDBUS6" org.kde.KWin /Scripting \
        start >>"$LOG_DIR/kwin-script.log" 2>&1 \
        || die "KWin window-placement start failed for $phase/$variant"
    sleep 0.5
}

start_variant() {
    local phase=$1 variant=$2 binary=$3 filter=$4 backend=${5:-}
    local config="$CONFIG_DIR/$phase/$variant" ack="$RUN_DIR/$phase-$variant-ack.log"
    local stdout="$LOG_DIR/$phase-$variant.stdout"
    write_config "$config" "$filter" "$backend"
    : >"$ack"
        RIO_CONFIG_HOME="$config" RIO_LOG_LEVEL=INFO RIO_ACCEPT_ACK_FILE="$ack" \
        RIO_ACCEPT_FIXTURE_HOLD="${RIO_ACCEPT_FIXTURE_HOLD:-8}" WAYLAND_DISPLAY="$SOCKET" \
        DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" XDG_SESSION_TYPE=wayland \
        env -u DISPLAY nohup setsid "$binary" --enable-log-file \
        --title-placeholder "gpu-$phase-$variant" --app-id "gpu-$phase-$variant" \
        -e /bin/sh "$FIXTURE" >"$stdout" 2>&1 &
    STARTED_PID=$!
    PIDS+=("$STARTED_PID")
    wait_for_text "$ack" READY "$phase/$variant shell"
    wait_until "$phase/$variant Rio startup" 30 \
        "[[ -f \"$config/log/rio.log\" ]] && grep -Fq 'Initialisation complete' \"$config/log/rio.log\""
    GUI_PID=$(rio_process_for "$STARTED_PID" "$binary") \
        || die "Rio GUI process not found for $phase/$variant"
    place_window "$GUI_PID" "$phase" "$variant"
    log "$phase/$variant gui_pid=$GUI_PID"
}

capture_content() {
    local phase=$1 variant=$2 state=$3
    local full="$RUN_DIR/$phase-$variant-$state-full.png"
    local content="$RUN_DIR/$phase-$variant-$state-content.png"
    if [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == yes ]]; then
        "$IMPORT" -window root "$full"
    else
        XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$SOCKET" \
            DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" QT_QPA_PLATFORM=wayland \
            env -u DISPLAY "$SPECTACLE" --background --new-instance --nonotify \
            --no-decoration --no-shadow --fullscreen --output "$full" \
            >"$LOG_DIR/spectacle.log" 2>&1 \
            || die "private Wayland screenshot failed for $phase/$variant/$state"
    fi
    "$IDENTIFY" "$full" >>"$RUN_DIR/summary.log"
    "$CONVERT" "$full" -crop 784x454+8+124 +repage "$content"
    "$IDENTIFY" "$content" >>"$RUN_DIR/summary.log"
}

capture_state_region() {
    local phase=$1 variant=$2 source_state=$3 output_state=$4 region=$5
    local source="$RUN_DIR/$phase-$variant-$source_state-content.png"
    local output="$RUN_DIR/$phase-$variant-$output_state-region.png"
    "$CONVERT" "$source" -crop "$region" +repage "$output"
    [[ -s "$output" ]] || die "state region capture was empty for $phase/$variant/$output_state"
}

positive_ae_metric() {
    local metric=$1
    [[ "$metric" =~ ^[0-9]+([.][0-9]+)?$ ]] || return 1
    awk -v value="$metric" 'BEGIN { exit !(value > 0) }'
}

assert_region_delta() {
    local phase=$1 variant=$2 first=$3 second=$4 label=$5 raw metric
    raw=$(
        "$COMPARE" -metric AE \
            "$RUN_DIR/$phase-$variant-$first-region.png" \
            "$RUN_DIR/$phase-$variant-$second-region.png" null: 2>&1 || true
    )
    metric=${raw%% *}
    printf '%s.%s.%s_region_ae=%s\n' "$phase" "$variant" "$label" "$raw" \
        >>"$RUN_DIR/summary.log"
    positive_ae_metric "$metric" \
        || die "$phase/$variant $label region did not change"
}

pixel_color_count() {
    local image=$1 color=$2
    local expression
    case "$color" in
        '#ff0000')
            expression='r > 0.45 && r > g*1.5 && r > b*1.5'
            ;;
        '#ff8000')
            expression='r > 0.45 && g > 0.2 && g < 0.85 && b < 0.35 && r > g*1.2 && g > b*1.5'
            ;;
        '#00ff00')
            expression='g > 0.45 && g > r*1.5 && g > b*1.5'
            ;;
        '#0000ff')
            expression='b > 0.45 && b > r*1.5 && b > g*1.5'
            ;;
        '#ffffff')
            expression='r > 0.45 && g > 0.45 && b > 0.45'
            ;;
        *)
            die "pixel oracle has no predicate for expected color $color"
            ;;
    esac
    "$CONVERT" "$image" -alpha off -colorspace RGB \
        -fx "$expression ? 1 : 0" -format '%[fx:mean*w*h]' info:
}

assert_pixel_color() {
    local phase=$1 variant=$2 state=$3 label=$4 color=$5 minimum=$6
    local image="$RUN_DIR/$phase-$variant-$state-region.png" count
    count=$(pixel_color_count "$image" "$color")
    printf '%s.%s.%s_pixel_oracle_%s=%s color=%s minimum=%s\n' \
        "$phase" "$variant" "$state" "$label" "$count" "$color" "$minimum" \
        >>"$RUN_DIR/summary.log"
    awk -v count="$count" -v minimum="$minimum" \
        'BEGIN { exit !(count >= minimum) }' \
        || die "$phase/$variant $label pixel oracle found too few $color pixels"
}

state_delta() {
    local phase=$1 variant=$2 state=$3 raw metric
    raw=$("$COMPARE" -metric AE \
        "$RUN_DIR/$phase-$variant-base-content.png" \
        "$RUN_DIR/$phase-$variant-$state-content.png" null: 2>&1 || true)
    metric=${raw%% *}
    printf '%s.%s.%s_content_ae=%s\n' "$phase" "$variant" "$state" "$raw" \
        >>"$RUN_DIR/summary.log"
    positive_ae_metric "$metric" \
        || die "$phase/$variant $state did not change the cropped terminal content"
}

run_visual_variant() {
    local phase=$1 variant=$2 binary=$3 filter=$4
    local ack
    start_variant "$phase" "$variant" "$binary" "$filter"
    ack="$RUN_DIR/$phase-$variant-ack.log"
    for state in "${VISUAL_STATES[@]}"; do
        wait_for_text "$ack" "STATE=$state" "$phase/$variant $state fixture state"
        sleep 0.75
        capture_content "$phase" "$variant" "$state"
        [[ "$state" == base ]] || state_delta "$phase" "$variant" "$state"
    done
    # Fixture rows 10-14 contain the OSC-4 mutation and advanced underline set.
    for state in base palette underline; do
        capture_state_region "$phase" "$variant" "$state" "$state" '784x180+0+250'
    done
    assert_region_delta "$phase" "$variant" base palette osc4_palette_mutation
    assert_region_delta "$phase" "$variant" base underline advanced_underline_styles
    if [[ "$filter" == none ]]; then
        assert_pixel_color "$phase" "$variant" palette osc4_mutated_red '#ff0000' 8
        assert_pixel_color "$phase" "$variant" underline colored_underline '#ff8000' 1

        capture_state_region "$phase" "$variant" kitty kitty-red "$KITTY_RED_REGION"
        capture_state_region "$phase" "$variant" kitty kitty-green "$KITTY_GREEN_REGION"
        capture_state_region "$phase" "$variant" kitty kitty-blue "$KITTY_BLUE_REGION"
        capture_state_region "$phase" "$variant" kitty kitty-white "$KITTY_WHITE_REGION"
        assert_pixel_color "$phase" "$variant" kitty-red kitty_red '#ff0000' 128
        assert_pixel_color "$phase" "$variant" kitty-green kitty_green '#00ff00' 128
        assert_pixel_color "$phase" "$variant" kitty-blue kitty_blue '#0000ff' 128
        assert_pixel_color "$phase" "$variant" kitty-white kitty_white '#ffffff' 128

        capture_state_region "$phase" "$variant" sixel sixel-red "$SIXEL_RED_REGION"
        capture_state_region "$phase" "$variant" sixel sixel-blue "$SIXEL_BLUE_REGION"
        assert_pixel_color "$phase" "$variant" sixel-red sixel_red '#ff0000' 64
        assert_pixel_color "$phase" "$variant" sixel-blue sixel_blue '#0000ff' 64
    else
        printf '%s.%s.pixel_oracle=SKIPPED: filtered capture; canonical image-color checks use native unfiltered output only\n' \
            "$phase" "$variant" >>"$RUN_DIR/summary.log"
    fi
    wait_for_text "$ack" DONE "$phase/$variant fixture completion"
    stop_tree "$STARTED_PID"
    log "$phase/$variant visual states captured with GPU rendering"
}

run_selection_variant() {
    local variant=$1 binary=$2 phase=selection ack window raw metric
    start_variant "$phase" "$variant" "$binary" none webgpu
    ack="$RUN_DIR/$phase-$variant-ack.log"
    wait_for_text "$ack" STATE=base "$phase/$variant selection fixture state"
    sleep 0.75
    window=$(window_for_pid "$GUI_PID" || true)
    if [[ ! "$window" =~ ^[0-9]+$ ]] &&
        [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == yes ]]; then
        window=$("$XDOTTOOL" getactivewindow 2>/dev/null || true)
        if [[ ! "$window" =~ ^[0-9]+$ ]]; then
            while read -r candidate; do
                window=$candidate
                break
            done < <("$XDOTTOOL" search --onlyvisible --name '.*' 2>/dev/null || true)
        fi
        log "selection/$variant using private X11 host window=$window"
    fi
    [[ "$window" =~ ^[0-9]+$ ]] || die "GPU selection window was not found for $variant"
    capture_content "$phase" "$variant" before
    "$XDOTTOOL" windowactivate --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" key --window "$window" --clearmodifiers ctrl+shift+a
    sleep 1
    capture_content "$phase" "$variant" after
    for state in before after; do
        capture_state_region "$phase" "$variant" "$state" "$state" '784x220+0+0'
    done
    assert_region_delta "$phase" "$variant" before after selection_highlight
    raw=$(
        "$COMPARE" -metric AE \
            "$RUN_DIR/$phase-$variant-before-content.png" \
            "$RUN_DIR/$phase-$variant-after-content.png" null: 2>&1 || true
    )
    metric=${raw%% *}
    printf '%s.%s.selection_content_ae=%s\n' "$phase" "$variant" "$raw" \
        >>"$RUN_DIR/summary.log"
    positive_ae_metric "$metric" \
        || die "GPU selection did not change the captured content for $variant"
    printf 'selection=CAPTURED variant=%s\n' "$variant" >>"$RUN_DIR/summary.log"
    wait_for_text "$ack" DONE "$phase/$variant selection fixture completion"
    stop_tree "$STARTED_PID"
    log "$phase/$variant GPU selection capture completed"
}

run_profile_variant() {
    local phase=$1 variant=$2 binary=$3 filter=$4
    local config="$CONFIG_DIR/profile-$phase/$variant" ack="$RUN_DIR/profile-$phase-$variant-ack.log"
    local stdout="$LOG_DIR/profile-$phase-$variant.stdout"
    local gpu_pid gpu_log
    write_config "$config" "$filter"
    : >"$ack"
    RIO_CONFIG_HOME="$config" RIO_LOG_LEVEL=INFO RIO_ACCEPT_ACK_FILE="$ack" \
        RIO_ACCEPT_PROFILE=1 WAYLAND_DISPLAY="$SOCKET" \
        DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" XDG_SESSION_TYPE=wayland \
        env -u DISPLAY nohup setsid "$binary" --enable-log-file \
        --title-placeholder "gpu-profile-$phase-$variant" \
        --app-id "gpu-profile-$phase-$variant" -e /bin/sh "$FIXTURE" \
        >"$stdout" 2>&1 &
    profile_root=$!
    PIDS+=("$profile_root")
    wait_for_text "$ack" READY "profile $phase/$variant shell"
    wait_until "profile $phase/$variant Rio startup" 30 \
        "[[ -f \"$config/log/rio.log\" ]] && grep -Fq 'Initialisation complete' \"$config/log/rio.log\""
    profile_gui=$(rio_process_for "$profile_root" "$binary") \
        || die "profile Rio GUI process not found for $phase/$variant"
    place_window "$profile_gui" "profile-$phase" "$variant"
    wait_for_text "$ack" PROFILE_START "profile $phase/$variant workload start"
    gpu_log="$RUN_DIR/profile-$phase-$variant-radeontop.log"
    "$RADEONTOP" -d "$gpu_log" -i 1 -l 4 >"$LOG_DIR/profile-$phase-$variant-radeontop.stdout" 2>&1 &
    gpu_pid=$!
    PIDS+=("$gpu_pid")
    sleep 1
    {
        printf 'profile=%s variant=%s phase=%s\n' "$phase" "$variant" "$phase"
        ps -p "$profile_gui" -o pid=,ppid=,%cpu=,rss=,args=
        for pid in $(owned_descendants "$profile_root"); do
            ps -p "$pid" -o pid=,ppid=,%cpu=,rss=,args= 2>/dev/null || true
        done
    } >"$RUN_DIR/profile-$phase-$variant-before.txt"
    sleep 2
    {
        printf 'profile=%s variant=%s phase=%s\n' "$phase" "$variant" "$phase"
        ps -p "$profile_gui" -o pid=,ppid=,%cpu=,rss=,args=
        for pid in $(owned_descendants "$profile_root"); do
            ps -p "$pid" -o pid=,ppid=,%cpu=,rss=,args= 2>/dev/null || true
        done
    } >"$RUN_DIR/profile-$phase-$variant-after.txt"
    wait_for_text "$ack" PROFILE_FRAMES "profile $phase/$variant workload completion"
    grep -F 'PROFILE_FRAMES=' "$ack" >>"$RUN_DIR/summary.log"
    kill "$gpu_pid" 2>/dev/null || true
    wait "$gpu_pid" 2>/dev/null || true
    [[ -s "$gpu_log" ]] || die "GPU activity sample is empty for profile $phase/$variant"
    grep -Eiq 'gpu|shader|vram|bus' "$gpu_log" \
        || die "GPU activity sample has no RadeonTop metrics for profile $phase/$variant"
    printf 'gpu_activity_log=%s\n' "$gpu_log" >>"$RUN_DIR/summary.log"
    stop_tree "$profile_root"
    log "$phase/$variant bounded GPU workload snapshot captured"
}

run_present_timing_probe() {
    local log="$RUN_DIR/present-timing-vkcube.log" status
    if [[ ! -x "$VKCUBE" ]]; then
        printf '%s\n' \
            "presentation_probe=UNAVAILABLE: vkcube is not installed at $VKCUBE" \
            >>"$RUN_DIR/summary.log"
        return
    fi
    if ! command -v timeout >/dev/null 2>&1; then
        printf '%s\n' \
            'presentation_probe=UNAVAILABLE: timeout command is not installed' \
            >>"$RUN_DIR/summary.log"
        return
    fi
    if HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/kwin-config" \
        XDG_DATA_HOME="$RUN_DIR/kwin-data" XDG_CACHE_HOME="$RUN_DIR/kwin-cache" \
        DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" WAYLAND_DISPLAY="$SOCKET" \
        timeout 30s "$VKCUBE" --wsi wayland --display_timing --c 60 \
        >"$log" 2>&1; then
        status=0
    else
        status=$?
    fi
    if grep -Fq 'VK_GOOGLE_display_timing extension NOT AVAILABLE' "$log"; then
        printf '%s\n' \
            'presentation_probe=UNAVAILABLE: private vkcube reported VK_GOOGLE_display_timing extension NOT AVAILABLE; no present timestamps' \
            >>"$RUN_DIR/summary.log"
    elif grep -Fq 'VK_GOOGLE_display_timing extension enabled' "$log"; then
        printf '%s\n' \
            "presentation_probe=CAPABILITY_ONLY: vkcube enabled VK_GOOGLE_display_timing but emitted no interval records; exit_status=$status" \
            >>"$RUN_DIR/summary.log"
    else
        printf '%s\n' \
            "presentation_probe=UNAVAILABLE: private vkcube exit_status=$status without a display-timing result" \
            >>"$RUN_DIR/summary.log"
    fi
}

record_gpu_startup() {
    local rio_log count=0
    for rio_log in "$CONFIG_DIR"/*/*/log/rio.log; do
        grep -E 'Vulkan device created:|Swapchain:|Selected adapter:|Surface format:' \
            "$rio_log" >>"$RUN_DIR/summary.log" \
            || die "GPU startup evidence missing from $rio_log"
        count=$((count + 1))
    done
    log "GPU startup evidence captured from $count Rio logs"
}

require_tools() {
    local command
    for command in "$KWIN" "$DBUS_DAEMON" "$CONVERT" "$COMPARE" "$IDENTIFY" \
        "$QDBUS6" "$VULKANINFO" "$RADEONTOP" \
        "$WAYLAND_INFO" pgrep ps grep awk; do
        require_command "$command"
    done
    if [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == yes ]]; then
        for command in "$XVFB" "$XDPYINFO" "$IMPORT" "$XDOTTOOL"; do
            require_command "$command"
        done
    else
        require_command "$SPECTACLE"
    fi
    [[ -x "$RIO_CURRENT_BIN" ]] || die "current Rio binary is not executable: $RIO_CURRENT_BIN"
    [[ -x "$RIO_UPSTREAM_BIN" ]] || die "upstream Rio binary is not executable: $RIO_UPSTREAM_BIN"
    [[ -x "$FIXTURE" ]] || die "graphics fixture is not executable: $FIXTURE"
}

require_tools
log "run_dir=$RUN_DIR"
if [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == yes ]]; then
    log "display=$DISPLAY resolution=1600x1000 host=private-Xvfb/KWin"
else
    log "display=private KWin virtual framebuffer resolution=1600x1000"
fi
log "socket=$SOCKET renderer=GPU use-cpu=false"
if [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == yes ]]; then
    "$XVFB" "$DISPLAY" -screen 0 1600x1000x24 -nolisten tcp -ac >"$LOG_DIR/xvfb.log" 2>&1 &
    XVFB_PID=$!
    wait_until "private Xvfb" 15 "\"$XDPYINFO\" -display \"$DISPLAY\" >/dev/null 2>&1"
fi

DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS" "$DBUS_DAEMON" --session \
    --address="$BUS_ADDRESS" --nofork >"$LOG_DIR/dbus.log" 2>&1 &
DBUS_PID=$!
wait_until "private D-Bus" 15 "kill -0 $DBUS_PID 2>/dev/null"

kwin_args=(--socket="$SOCKET" --width=1600 --height=1000 --no-lockscreen \
    --no-global-shortcuts --no-kactivities)
kwin_env=(HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/kwin-config" \
    XDG_DATA_HOME="$RUN_DIR/kwin-data" XDG_CACHE_HOME="$RUN_DIR/kwin-cache" \
    DBUS_SESSION_BUS_ADDRESS="$BUS_ADDRESS")
if [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == yes ]]; then
    kwin_args+=(--x11-display="$DISPLAY")
    kwin_env+=(QT_QPA_PLATFORM=xcb LIBGL_ALWAYS_SOFTWARE=1)
else
    kwin_args+=(--virtual)
fi
env "${kwin_env[@]}" "$KWIN" "${kwin_args[@]}" >"$LOG_DIR/kwin.log" 2>&1 &
kwin_pid=$!
PIDS+=("$kwin_pid")
wait_until "private KWin Wayland socket" 15 \
    "[[ -S \"$RUNTIME_DIR/$SOCKET\" ]] && kill -0 $kwin_pid 2>/dev/null"
if XDG_RUNTIME_DIR="$RUNTIME_DIR" "$VULKANINFO" --summary >"$LOG_DIR/vulkaninfo.log" 2>&1; then
    log "vulkaninfo=PASS"
else
    vulkaninfo_status=$?
    log "UNSUPPORTED: vulkaninfo exited with status=$vulkaninfo_status; Rio startup logs remain the GPU evidence"
fi
run_present_timing_probe
XDG_RUNTIME_DIR="$RUNTIME_DIR" WAYLAND_DISPLAY="$SOCKET" "$WAYLAND_INFO" \
    >"$LOG_DIR/wayland-info.log" 2>&1 || die "private Wayland global enumeration failed"
grep -Eiq 'wl_compositor|xdg_wm_base' "$LOG_DIR/wayland-info.log" \
    || die "private Wayland globals are incomplete"

if [[ "$RIO_ACCEPT_GPU_SELECTION_ONLY" == 1 || "$RIO_ACCEPT_GPU_SELECTION_ONLY" == true || "$RIO_ACCEPT_GPU_SELECTION_ONLY" == yes ]]; then
    [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == yes ]] \
        || die "GPU selection-only mode requires RIO_ACCEPT_KWIN_X11=1"
    run_selection_variant current "$RIO_CURRENT_BIN"
    run_selection_variant upstream "$RIO_UPSTREAM_BIN"
    record_gpu_startup
    printf '%s\n' \
        'selection=CAPTURED: public SelectAll changed dedicated content regions under private X11/KWin' \
        'ime=UNVERIFIED: text-input/input-method protocols and KWin --inputmethod are installed, but no ready private input-method server is available; preedit and commit are not claimed' \
        'note=selection-only mode uses WGPU to avoid native Vulkan X11 surface limitations' \
        >>"$RUN_DIR/summary.log"
    log "PASS: private X11/KWin GPU selection regions completed"
    exit 0
fi

for variant in current upstream; do
    binary="$RIO_CURRENT_BIN"
    [[ "$variant" == upstream ]] && binary="$RIO_UPSTREAM_BIN"
    run_visual_variant plain "$variant" "$binary" none
    run_visual_variant filter "$variant" "$binary" "${RIO_ACCEPT_GPU_FILTER:-newpixiecrt}"
    run_profile_variant plain "$variant" "$binary" none
    run_profile_variant filter "$variant" "$binary" "${RIO_ACCEPT_GPU_FILTER:-newpixiecrt}"
done

if [[ "$USE_PRIVATE_X11" == 1 || "$USE_PRIVATE_X11" == true || "$USE_PRIVATE_X11" == yes ]]; then
    run_selection_variant current "$RIO_CURRENT_BIN"
    run_selection_variant upstream "$RIO_UPSTREAM_BIN"
fi

for phase in plain filter; do
    for state in "${VISUAL_STATES[@]}"; do
        raw=$("$COMPARE" -metric AE \
            "$RUN_DIR/$phase-current-$state-content.png" \
            "$RUN_DIR/$phase-upstream-$state-content.png" null: 2>&1 || true)
        printf '%s.cross_variant.%s_content_ae=%s\n' "$phase" "$state" "$raw" \
            >>"$RUN_DIR/summary.log"
    done
done

filter_delta_count=0
for state in "${VISUAL_STATES[@]}"; do
    raw=$("$COMPARE" -metric AE \
        "$RUN_DIR/plain-current-$state-content.png" \
        "$RUN_DIR/filter-current-$state-content.png" null: 2>&1 || true)
    printf 'current.filter_delta.%s_content_ae=%s\n' "$state" "$raw" >>"$RUN_DIR/summary.log"
    metric=${raw%% *}
    if positive_ae_metric "$metric"; then
        filter_delta_count=$((filter_delta_count + 1))
    fi
done
(( filter_delta_count > 0 )) \
    || die "configured newpixiecrt produced no content-crop delta in any captured state"

record_gpu_startup

printf '%s\n' \
    'graphics_fixture=CAPTURED: Kitty RGBA quadrants and Sixel red/blue bands emitted under live GPU rendering' \
    'osc4=CAPTURED: palette index mutation changed a dedicated content region' \
    'underline=CAPTURED: double, curly, dotted, dashed, and colored underline regions changed' \
    'filters=CAPTURED: configured newpixiecrt path under live GPU rendering' \
    'ime=UNVERIFIED: text-input/input-method protocols and KWin --inputmethod are installed, but no ready private input-method server is available; preedit and commit are not claimed' \
    'input=UNVERIFIED: this visual harness does not claim native pointer or key delivery' \
    'perf=BOUNDED_ACTIVITY: profile frame count plus two CPU/RSS snapshots per variant/phase' \
    'presentation_timing=UNMEASURED: private KWin/Xvfb and Rio logs expose no presentation or swapchain-completion timestamps; no FPS, frame-latency, or input-latency claim' \
    'note=content crops and AE metrics are evidence; they are not whole-window pixel identity' \
    >>"$RUN_DIR/summary.log"
if [[ "$USE_PRIVATE_X11" != 1 && "$USE_PRIVATE_X11" != true && "$USE_PRIVATE_X11" != yes ]]; then
    printf '%s\n' \
        'selection=UNVERIFIED: GPU selection capture requires private Xvfb/KWin-X11 mode' \
        >>"$RUN_DIR/summary.log"
fi
log "PASS: private Wayland GPU visual states, graphics protocol fixtures, filter path, and bounded workload snapshots completed"
log "UNVERIFIED: no ready private input-method server was available for IME preedit/commit; native keyboard delivery remains covered by separate acceptance paths"
