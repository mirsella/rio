#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# This harness deliberately uses only a private X server. It never searches for
# or signals an unrelated desktop process.

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARTIFACT_ROOT=${RIO_ACCEPT_ARTIFACT_ROOT:-"$HOME/dev/rio-agent-artifacts"}
RUN_ID="session-isolation-$(date -u +%Y%m%dT%H%M%SZ)-$$"
RUN_DIR="$ARTIFACT_ROOT/$RUN_ID"
LOG_DIR="$RUN_DIR/logs"
CONFIG_DIR="$RUN_DIR/config"
# Unix-domain session endpoints include the runtime path and a generated
# session name. Keep these two private paths short enough for sockaddr_un.
RUNTIME_DIR="$ARTIFACT_ROOT/r-$$"
TMP_DIR="$ARTIFACT_ROOT/t-$$"

RIO_BIN=${RIO_BIN:-"$ROOT/target/debug/rio"}
XVFB=${XVFB:-/usr/bin/Xvfb}
XDOTTOOL=${XDOTTOOL:-/usr/bin/xdotool}
XPROP=${XPROP:-/usr/bin/xprop}
XDPYINFO=${XDPYINFO:-/usr/bin/xdpyinfo}
IMPORT=${RIO_ACCEPT_IMPORT:-/usr/bin/import}
CONVERT=${RIO_ACCEPT_CONVERT:-/usr/bin/convert}
COMPARE=${RIO_ACCEPT_COMPARE:-/usr/bin/compare}
IDENTIFY=${RIO_ACCEPT_IDENTIFY:-/usr/bin/identify}
SHELL_FIXTURE="$ROOT/scripts/fixtures/session-isolation-ack-shell.sh"
RIO_ACCEPT_USE_CPU=${RIO_ACCEPT_USE_CPU:-1}
RIO_ACCEPT_MIN_IMAGE_DELTA=${RIO_ACCEPT_MIN_IMAGE_DELTA:-1000}
RIO_ACCEPT_READY_TIMEOUT=${RIO_ACCEPT_READY_TIMEOUT:-45}

mkdir -p "$LOG_DIR" "$RUNTIME_DIR" "$CONFIG_DIR" "$TMP_DIR"
chmod 700 "$RUNTIME_DIR" "$CONFIG_DIR" "$TMP_DIR"

export TMPDIR="$TMP_DIR"
export XDG_RUNTIME_DIR="$RUNTIME_DIR"
export RIO_CONFIG_HOME="$CONFIG_DIR"
unset WAYLAND_DISPLAY
unset WAYLAND_SOCKET

PIDS=()
XVFB_PID=
DISPLAY_NUM=${RIO_ACCEPT_DISPLAY_NUM:-$((100 + ($$ % 800)))}
export DISPLAY=":$DISPLAY_NUM"

trap cleanup EXIT INT TERM

wait_for_file_text() {
    local file=$1 text=$2 timeout=${3:-10}
    wait_for_text "$file" "$text" "'$text' in $file" "$timeout"
}

window_for_title() {
    local title=$1
    mapfile -t _windows < <("$XDOTTOOL" search --onlyvisible --name "^${title}$" 2>/dev/null || true)
    ((${#_windows[@]} > 0)) || return 1
    printf '%s\n' "${_windows[0]}"
}

wait_for_window() {
    local title=$1 timeout=${2:-20} result
    wait_until "window '$title'" "$timeout" "result=\$(window_for_title \"$title\" 2>/dev/null || true); [[ -n \"\$result\" ]]"
    window_for_title "$title"
}

visible_window_ids() {
    mapfile -t _visible_windows < <("$XDOTTOOL" search --onlyvisible --name '.*' 2>/dev/null || true)
    printf '%s\n' "${_visible_windows[@]}"
}

wait_for_new_window() {
    local old_window=$1 timeout=${2:-20} result window pid known_window
    shift 2
    local -a known_windows=("$@")
    local deadline=$((SECONDS + timeout))
    while ((SECONDS < deadline)); do
        result=$(
            while read -r window; do
                [[ -n "$window" && "$window" != "$old_window" ]] || continue
                for known_window in "${known_windows[@]}"; do
                    [[ "$window" == "$known_window" ]] && continue 2
                done
                pid=$(window_pid "$window" 2>/dev/null || true)
                [[ "$pid" =~ ^[0-9]+$ ]] && {
                    printf '%s\n' "$window"
                    break
                }
            done < <(visible_window_ids)
        )
        if [[ -n "$result" ]]; then
            printf '%s\n' "$result"
            return 0
        fi
        sleep 0.1
    done
    die "timed out waiting for a new private Rio window"
}

send_line() {
    local window=$1 line=$2
    "$XDOTTOOL" windowactivate --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" windowfocus --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" type --clearmodifiers --delay 1 --window "$window" "$line"
    "$XDOTTOOL" key --window "$window" Return
}

palette_action() {
    local window=$1 query=$2
    "$XDOTTOOL" windowactivate --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" windowfocus --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" key --window "$window" ctrl+shift+p
    sleep 0.35
    "$XDOTTOOL" type --clearmodifiers --delay 2 --window "$window" "$query"
    sleep 0.35
    "$XDOTTOOL" key --window "$window" Return
    sleep 0.8
}

start_gui() {
    local role=$1 title=$2 ack_file=$3 log_file=$4
    RIO_CONFIG_HOME="$CONFIG_DIR/$role" RIO_LOG_LEVEL=INFO RIO_ACCEPT_ACK_FILE="$ack_file" \
        "$RIO_BIN" --enable-log-file --title-placeholder "$title" --app-id "rio-accept-$role" \
        -e /bin/sh "$SHELL_FIXTURE" >"$log_file" 2>&1 &
    local pid=$!
    PIDS+=("$pid")
    STARTED_PID=$pid
}

measure_processes() {
    local label=$1 root=$2 pid
    {
        printf '\n[%s] process snapshot (%s)\n' "$(date -u +%H:%M:%S)" "$label"
        printf 'pid ppid cpu_percent rss_kib command\n'
        mapfile -t _measure_tree < <(printf '%s\n' "$root"; owned_descendants "$root")
        for pid in "${_measure_tree[@]}"; do
            [[ "$pid" =~ ^[0-9]+$ ]] || continue
            ps -p "$pid" -o pid=,ppid=,%cpu=,rss=,args= 2>/dev/null || true
        done
    } | tee -a "$RUN_DIR/summary.log"
}

capture_window() {
    local window=$1 output=$2
    "$IMPORT" -window "$window" "$output" >/dev/null 2>&1 ||
        die "could not capture private window $window to $output"
    [[ -s "$output" ]] || die "private window capture was empty: $output"
}

capture_region() {
    local input=$1 output=$2 geometry=$3
    "$CONVERT" "$input" -crop "$geometry" +repage "$output"
    [[ -s "$output" ]] || die "private image region was empty: $output"
}

assert_region_changed() {
    local description=$1 first=$2 second=$3 metric
    metric=$(
        "$COMPARE" -metric AE "$first" "$second" null: 2>&1 || true
    )
    metric=${metric%% *}
    printf '%s=%s\n' "$description" "$metric" >>"$RUN_DIR/summary.log"
    [[ "$metric" != 0 && "$metric" != 0.0 ]] \
        || die "$description did not change"
}

assert_region_unchanged() {
    local description=$1 first=$2 second=$3 metric
    metric=$(
        "$COMPARE" -metric AE "$first" "$second" null: 2>&1 || true
    )
    metric=${metric%% *}
    printf '%s=%s\n' "$description" "$metric" >>"$RUN_DIR/summary.log"
    [[ "$metric" == 0 || "$metric" == 0.0 ]] \
        || die "$description changed"
}

image_changed() {
    local first=$1 second=$2 metric
    metric=$("$COMPARE" -metric AE "$first" "$second" null: 2>&1 || true)
    metric=${metric%%$'\n'*}
    metric=${metric%% *}
    metric=${metric%%.*}
    [[ "$metric" =~ ^[0-9]+$ ]] && ((metric >= RIO_ACCEPT_MIN_IMAGE_DELTA))
}

wait_for_image_change() {
    local description=$1 first=$2 second=$3 window=${4:-$destination_window}
    local deadline=$((SECONDS + 6))
    while ((SECONDS < deadline)); do
        capture_window "$window" "$second"
        if image_changed "$first" "$second"; then
            log "$description observed"
            return 0
        fi
        sleep 0.2
    done
    die "timed out waiting for $description"
}

wait_for_region_change() {
    local description=$1 first=$2 second=$3 geometry=$4 output=$5
    local window=${6:-$destination_window} deadline=$((SECONDS + 6))
    while ((SECONDS < deadline)); do
        capture_window "$window" "$second"
        capture_region "$second" "$output" "$geometry"
        if image_changed "$first" "$output"; then
            log "$description observed"
            return 0
        fi
        sleep 0.2
    done
    die "timed out waiting for $description"
}

image_dimensions_changed() {
    local before=$1 after=$2
    [[ -f "$before" && -f "$after" ]] || return 1
    [[ "$($IDENTIFY -format '%wx%h' "$before")" != "$($IDENTIFY -format '%wx%h' "$after")" ]]
}

for command in "$XVFB" "$XDOTTOOL" "$XPROP" "$XDPYINFO" "$IMPORT" "$CONVERT" "$COMPARE" "$IDENTIFY" pgrep ps awk grep; do
    require_command "$command"
done
[[ -x "$RIO_BIN" ]] || die "RIO_BIN is not executable: $RIO_BIN"
[[ -x "$SHELL_FIXTURE" ]] || die "shell fixture is not executable: $SHELL_FIXTURE"

log "run_dir=$RUN_DIR"
log "binary=$RIO_BIN"
log "display=$DISPLAY"
log "runtime=$XDG_RUNTIME_DIR"
log "tmpdir=$TMPDIR"
log "renderer.use-cpu=$RIO_ACCEPT_USE_CPU"

"$XVFB" "$DISPLAY" -screen 0 1600x900x24 -nolisten tcp -noreset \
    >"$LOG_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
PIDS+=("$XVFB_PID")
wait_until "private X server" 10 "kill -0 $XVFB_PID 2>/dev/null && $XDPYINFO >/dev/null 2>&1"

use_cpu=false
case "$RIO_ACCEPT_USE_CPU" in
    1|true|TRUE|yes) use_cpu=true ;;
esac
for role in source destination; do
    mkdir -p "$CONFIG_DIR/$role"
    : >"$CONFIG_DIR/$role/config.toml"
    if [[ "$role" == source ]]; then
        printf 'shell = { program = "/bin/sh", args = ["%s"] }\n' \
            "$SHELL_FIXTURE" >>"$CONFIG_DIR/$role/config.toml"
    fi
    printf '%s\n' \
        'draw-bold-text-with-light-colors = true' \
        '' \
        '[renderer]' \
        "use-cpu = $use_cpu" \
        '' \
        '[developer]' \
        'log-level = "INFO"' \
        >>"$CONFIG_DIR/$role/config.toml"
done

source_ack="$RUN_DIR/source.ack"
destination_ack="$RUN_DIR/destination.ack"
source_log="$LOG_DIR/source.log"
destination_log="$LOG_DIR/destination.log"
source_rio_log="$CONFIG_DIR/source/log/rio.log"
destination_rio_log="$CONFIG_DIR/destination/log/rio.log"
source_title="rio-accept-source"
destination_title="rio-accept-destination"

start_gui source "$source_title" "$source_ack" "$source_log"
source_pid=$STARTED_PID
start_gui destination "$destination_title" "$destination_ack" "$destination_log"
destination_pid=$STARTED_PID
source_window=$(wait_for_window "$source_title")
destination_window=$(wait_for_window "$destination_title")
log "private windows source=$source_window source_pid=$(window_pid "$source_window" || true) destination=$destination_window destination_pid=$(window_pid "$destination_window" || true)"
# Keep both private surfaces exposed. Xvfb has no window manager, so two
# default-position windows can otherwise occlude the source and suppress its
# normal redraw path.
"$XDOTTOOL" windowmove "$source_window" 0 0
"$XDOTTOOL" windowsize "$source_window" 800 490
"$XDOTTOOL" windowmove "$destination_window" 800 0
"$XDOTTOOL" windowsize "$destination_window" 800 490
wait_for_file_text "$source_ack" READY
wait_for_file_text "$destination_ack" READY
source_shell_pid=$(awk -F'[ =]' '/^READY / {print $3; exit}' "$source_ack")
destination_shell_pid=$(awk -F'[ =]' '/^READY / {print $3; exit}' "$destination_ack")
[[ "$source_shell_pid" =~ ^[0-9]+$ ]] || die "source shell PID was not reported"
[[ "$destination_shell_pid" =~ ^[0-9]+$ ]] || die "destination shell PID was not reported"

send_line "$source_window" ack
send_line "$destination_window" ack
wait_for_file_text "$source_ack" "ACK pid=$source_shell_pid count=1"
wait_for_file_text "$destination_ack" "ACK pid=$destination_shell_pid count=1"
log "initial shell ACK continuity verified source=$source_shell_pid destination=$destination_shell_pid"
log "initial direct Sugarloaf windows are live; readiness is checked by frame capture and ACK continuity"

source_initial="$RUN_DIR/source-initial.png"
source_initial_tabs="$RUN_DIR/source-initial-tabs.png"
source_nested="$RUN_DIR/source-nested.png"
send_line "$source_window" tab-keep
wait_for_file_text "$source_ack" "ACK pid=$source_shell_pid input=tab-keep"
capture_window "$source_window" "$source_initial"
capture_region "$source_initial" "$source_initial_tabs" '800x48+0+0'

# Build a source with multiple tabs and nested splits through the public palette.
palette_action "$source_window" "New Tab"
wait_until "new source tab shell" 10 \
    "(( \$(grep -Fc READY \"$source_ack\") >= 2 ))"
palette_action "$source_window" "Split Right"
wait_until "right source split shell" 10 \
    "(( \$(grep -Fc READY \"$source_ack\") >= 3 ))"
palette_action "$source_window" "Split Down"
wait_until "down source split shell" 10 \
    "(( \$(grep -Fc READY \"$source_ack\") >= 4 ))"
log "multi-tab and nested split setup completed through the command palette"
log "nested split layout prepared through the public command palette"
source_selected_shell_pid=$(awk -F'[ =]' '/^READY / {pid=$3} END {print pid}' "$source_ack")
[[ "$source_selected_shell_pid" =~ ^[0-9]+$ ]] || die "active post-layout source shell PID was not reported"
[[ "$source_selected_shell_pid" != "$source_shell_pid" ]] || die "source tab did not create a distinct shell"
mapfile -t selected_shell_pids < <(
    awk -F'[ =]' -v retained="$source_shell_pid" \
        '/^READY / && $3 != retained {print $3}' "$source_ack"
)
(( ${#selected_shell_pids[@]} >= 3 )) \
    || die "selected source tab did not retain all split-pane shell PIDs"
send_line "$source_window" tab-transfer
wait_for_file_text "$source_ack" "ACK pid=$source_selected_shell_pid input=tab-transfer"
log "active selected source tab ACK continuity verified source=$source_selected_shell_pid"
capture_window "$source_window" "$source_nested"
source_nested_tabs="$RUN_DIR/source-nested-tabs.png"
capture_region "$source_nested" "$source_nested_tabs" '800x48+0+0'
assert_region_changed "source tab strip became visible" "$source_initial_tabs" "$source_nested_tabs"
wait_for_image_change "nested split geometry" "$source_initial" "$source_nested" "$source_window"

# Exercise cancellation while the authenticated target is armed. The image
# delta is the observable target highlight, not just successful key input.
merge_before="$RUN_DIR/merge-target-before.png"
merge_armed="$RUN_DIR/merge-target-armed.png"
merge_highlight="$RUN_DIR/merge-target-highlight.png"
merge_cancelled="$RUN_DIR/merge-target-cancelled.png"
merge_overlay_geometry='400x300+200+100'
merge_before_overlay="$RUN_DIR/merge-target-before-overlay.png"
merge_armed_overlay="$RUN_DIR/merge-target-armed-overlay.png"
merge_highlight_overlay="$RUN_DIR/merge-target-highlight-overlay.png"
merge_cancelled_overlay="$RUN_DIR/merge-target-cancelled-overlay.png"
"$XDOTTOOL" windowraise "$destination_window" 2>/dev/null || true
"$XDOTTOOL" windowfocus --sync "$destination_window" 2>/dev/null || true
sleep 0.4
capture_window "$destination_window" "$merge_before"
palette_action "$source_window" "Merge Tab"
sleep 1
capture_window "$destination_window" "$merge_armed"
capture_region "$merge_before" "$merge_before_overlay" "$merge_overlay_geometry"
capture_region "$merge_armed" "$merge_armed_overlay" "$merge_overlay_geometry"
assert_region_unchanged "no target highlight before pointer entry" \
    "$merge_before_overlay" "$merge_armed_overlay"
"$XDOTTOOL" mousemove --window "$destination_window" 120 20
wait_until "target-local pointer event" 6 \
    "[[ -f \"$destination_rio_log\" ]] && grep -Eq 'native merge target pointer (entered|moved)' \"$destination_rio_log\""
wait_for_region_change "authenticated target highlight" "$merge_before_overlay" \
    "$merge_highlight" "$merge_overlay_geometry" "$merge_highlight_overlay"
"$XDOTTOOL" windowraise "$source_window" 2>/dev/null || true
"$XDOTTOOL" windowfocus --sync "$source_window" 2>/dev/null || true
"$XDOTTOOL" windowactivate --sync "$source_window" 2>/dev/null || true
"$XDOTTOOL" key --window "$source_window" --clearmodifiers Escape
wait_until "authenticated target cancellation" 6 \
    "[[ -f \"$destination_rio_log\" ]] && (( \$(grep -Fc 'window overlay state updated' \"$destination_rio_log\") >= 2 ))"
wait_for_region_change "target highlight cancellation cleanup" "$merge_highlight_overlay" \
    "$merge_cancelled" "$merge_overlay_geometry" "$merge_cancelled_overlay"

# Repeat the public action and complete it with a target-local X11 click.
merge_commit_before="$RUN_DIR/merge-commit-before.png"
capture_window "$destination_window" "$merge_commit_before"
palette_action "$source_window" "Merge Tab"
sleep 1
owned_descendants "$source_pid"
"$XDOTTOOL" windowraise "$destination_window" 2>/dev/null || true
"$XDOTTOOL" windowfocus --sync "$destination_window" 2>/dev/null || true
"$XDOTTOOL" windowactivate --sync "$destination_window" 2>/dev/null || true
"$XDOTTOOL" mousemove --window "$destination_window" 120 20
sleep 0.8
"$XDOTTOOL" click --clearmodifiers 1
wait_for_file_text "$destination_rio_log" "native merge target clicked" 6

wait_until "source GUI remains after selected-tab merge" 15 "kill -0 $source_pid 2>/dev/null"
wait_until "retained source shell remains after selected-tab merge" 15 "kill -0 $source_shell_pid 2>/dev/null"
for selected_shell_pid in "${selected_shell_pids[@]}"; do
    wait_until "transferred selected shell $selected_shell_pid remains after selected-tab merge" 15 \
        "kill -0 $selected_shell_pid 2>/dev/null"
done
merged_destination="$RUN_DIR/merged-destination.png"
source_after_merge="$RUN_DIR/source-after-selected-merge.png"
capture_window "$destination_window" "$merged_destination"
"$XDOTTOOL" windowraise "$source_window" 2>/dev/null || true
"$XDOTTOOL" windowfocus --sync "$source_window" 2>/dev/null || true
"$XDOTTOOL" windowactivate --sync "$source_window" 2>/dev/null || true
sleep 0.4
capture_window "$source_window" "$source_after_merge"
capture_region "$merge_commit_before" "$RUN_DIR/merge-commit-before-tabs.png" '800x48+0+0'
capture_region "$merged_destination" "$RUN_DIR/merged-destination-tabs.png" '800x48+0+0'
capture_region "$merge_commit_before" "$RUN_DIR/merge-commit-before-grid.png" '800x442+0+48'
capture_region "$merged_destination" "$RUN_DIR/merged-destination-grid.png" '800x442+0+48'
assert_region_changed "destination tab strip after selected-tab merge" \
    "$RUN_DIR/merge-commit-before-tabs.png" "$RUN_DIR/merged-destination-tabs.png"
assert_region_changed "destination content grid below visible tabs" \
    "$RUN_DIR/merge-commit-before-grid.png" "$RUN_DIR/merged-destination-grid.png"
capture_region "$source_nested" "$RUN_DIR/source-before-selected-merge-tabs.png" '800x48+0+0'
capture_region "$source_after_merge" "$RUN_DIR/source-after-selected-merge-tabs.png" '800x48+0+0'
assert_region_changed "source tab strip after selected-tab removal" \
    "$RUN_DIR/source-before-selected-merge-tabs.png" "$RUN_DIR/source-after-selected-merge-tabs.png"
log "target click committed only the selected tab; retained source tab and imported destination tab were captured"
send_line "$destination_window" ack
wait_for_file_text "$source_ack" "ACK pid=$source_selected_shell_pid count=1"
send_line "$source_window" tab-keep
wait_until "retained source tab ACK after selected-tab merge" 8 \
    "(( \$(grep -Fc 'ACK pid=$source_shell_pid input=tab-keep' \"$source_ack\") >= 2 ))"
log "selected transferred shell ACK and retained source tab ACK both remained live"

mapfile -t before_detach_tree < <(printf '%s\n' "$destination_pid"; owned_descendants "$destination_pid")
mapfile -t before_detach_windows < <(visible_window_ids)
palette_action "$destination_window" "Move Current Tab to New Window"
detached_window=$(wait_for_new_window "$destination_window" 20 "${before_detach_windows[@]}")
detached_pid=$(window_pid "$detached_window" 2>/dev/null || true)
if [[ ! "$detached_pid" =~ ^[0-9]+$ ]]; then
    mapfile -t after_detach_tree < <(printf '%s\n' "$destination_pid"; owned_descendants "$destination_pid")
    for candidate in "${after_detach_tree[@]}"; do
        [[ "$candidate" =~ ^[0-9]+$ ]] || continue
        [[ " ${before_detach_tree[*]} " == *" $candidate "* ]] && continue
        candidate_args=$(ps -p "$candidate" -o args= 2>/dev/null || true)
        if [[ "$candidate_args" == *"$RIO_BIN"* || "$candidate_args" == *"--window-bootstrap"* ]]; then
            detached_pid=$candidate
            break
        fi
    done
fi
[[ "$detached_pid" =~ ^[0-9]+$ ]] || die "detached window did not expose an owned native PID"
PIDS+=("$detached_pid")
log "detached target window=$detached_window name=$($XDOTTOOL getwindowname "$detached_window" 2>/dev/null || true) pid=$detached_pid"
for candidate in $(visible_window_ids); do
    candidate_name=$($XDOTTOOL getwindowname "$candidate" 2>/dev/null || true)
    candidate_pid=$(window_pid "$candidate" 2>/dev/null || true)
    log "visible window candidate=$candidate name=$candidate_name pid=${candidate_pid:-unknown}"
done
sleep 1
send_line "$detached_window" ack
sleep 0.5
send_line "$detached_window" ack
wait_for_file_text "$source_ack" "ACK pid=$source_selected_shell_pid count=3"
log "public detach preserved selected transferred shell PID=$source_selected_shell_pid"

# A resize exercises dynamic pane dimensions after the imported layout is live.
resize_before="$RUN_DIR/resize-before.png"
resize_after="$RUN_DIR/resize-after.png"
capture_window "$destination_window" "$resize_before"
"$XDOTTOOL" windowsize "$destination_window" 1280 720
sleep 1
capture_window "$destination_window" "$resize_after"
wait_until "direct-grid resize repaint" 8 "image_dimensions_changed \"$resize_before\" \"$resize_after\""
"$XDOTTOOL" key --window "$destination_window" ctrl+shift+o
sleep 0.5
"$XDOTTOOL" key --window "$destination_window" Escape
log "dynamic resize and hint mode repaint completed"

log "direct resident grids remained in the destination through resize and hint repaint"

measure_processes "post-acceptance idle" "$destination_pid"
log "PASS: private Xvfb acceptance completed"
