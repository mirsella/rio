#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# Capture a small deterministic direct-render comparison. This is evidence for
# the listed states only, not a complete pixel-parity or performance test.

ARTIFACT_ROOT=${RIO_ACCEPT_ARTIFACT_ROOT:-${HOME:?}/dev/rio-agent-artifacts}
RIO_CURRENT_BIN=${RIO_CURRENT_BIN:-$PWD/target/debug/rio}
RIO_UPSTREAM_BIN=${RIO_UPSTREAM_BIN:-$ARTIFACT_ROOT/target-upstream-direct-bench/debug/rio}
RUN_DIR=${ARTIFACT_ROOT}/session-isolation-visual-parity-$(date -u +%Y%m%dT%H%M%SZ)-$$
DISPLAY_NUM=$((100 + ($$ % 700)))
DISPLAY=:$DISPLAY_NUM
RUNTIME_DIR=$ARTIFACT_ROOT/r-$$
TMPDIR_RUN=$ARTIFACT_ROOT/t-$$
HOME_DIR=$RUN_DIR/home
CONFIG_DIR=$RUN_DIR/config
PIDS=()
ORPHANS=()
XVFB_PID=
OPENBOX_PID=
SI_LOG_PREFIX='[visual-parity] '

require_tools() {
    local tool
    for tool in Xvfb openbox xdpyinfo xdotool xprop import compare identify pgrep ps awk grep; do
        command -v "$tool" >/dev/null 2>&1 || die "missing required tool: $tool"
    done
    [[ -x "$RIO_CURRENT_BIN" ]] || die "current Rio binary is not executable: $RIO_CURRENT_BIN"
    [[ -x "$RIO_UPSTREAM_BIN" ]] || die "upstream Rio binary is not executable: $RIO_UPSTREAM_BIN"
}

trap cleanup EXIT INT TERM

wait_for_window() {
    local root=$1
    wait_until "window for $root" 30 "window_for_pid $root >/dev/null"
    window_for_pid "$root"
}

send_line() {
    local window=$1 line=$2 key index
    xdotool windowactivate --sync "$window" 2>/dev/null || true
    xdotool windowfocus --sync "$window" 2>/dev/null || true
    for ((index = 0; index < ${#line}; index++)); do
        key=${line:index:1}
        [[ "$key" == ' ' ]] && key=space
        xdotool key --clearmodifiers "$key"
    done
    xdotool key --clearmodifiers Return
}

capture() {
    local window=$1 name=$2
    DISPLAY="$DISPLAY" import -window "$window" "$RUN_DIR/$name.png"
    identify "$RUN_DIR/$name.png" >>"$RUN_DIR/summary.log"
}

state_delta() {
    local before=$1 after=$2 label=$3 metric raw
    raw=$(compare -metric AE "$RUN_DIR/$before.png" "$RUN_DIR/$after.png" null: 2>&1 || true)
    metric=${raw%% *}
    printf '%s=%s\n' "$label" "$metric" >>"$RUN_DIR/summary.log"
    [[ "$metric" != 0 && "$metric" != 0.0 ]] || die "$label did not change the private capture"
}

write_config() {
    local config=$1
    mkdir -p "$config/log"
    chmod 700 "$config"
    printf '%s\n' \
        'draw-bold-text-with-light-colors = true' \
        'confirm-before-quit = false' \
        '' \
        '[renderer]' \
        'use-cpu = true' \
        '' \
        '[bindings]' \
        'keys = [{ key = "a", with = "control|shift", action = "selectall" }]' \
        '' \
        '[developer]' \
        'log-level = "INFO"' >"$config/config.toml"
}

start_variant() {
    local variant=$1 binary=$2 x=$3
    local config="$CONFIG_DIR/$variant" ack="$RUN_DIR/$variant-ack.log"
    local script="$RUN_DIR/$variant-shell.sh" stdout="$RUN_DIR/$variant.stdout"
    write_config "$config"
    printf '%s\n' \
        '#!/bin/sh' \
        'ack_file=${1:-$RIO_ACCEPT_ACK_FILE}' \
        'printf "READY pid=%s\\n" "$$" > "$ack_file"' \
        'printf "\033[2J\033[H"' \
        'printf "direct render fixture\\n"' \
        'printf "\\033[31mred\\033[0m  \\033[1;34mbold-blue\\033[0m  \\033[4munderlined\\033[0m\\n"' \
        'printf "\\033]8;;https://example.invalid\\007link\\033]8;;\\007\\n"' \
        'printf "cursor-and-selection fixture\\n"' \
        'while IFS= read -r line; do' \
        '  case "$line" in' \
        '    alt) printf "\\033[?1049h\\033[2J\\033[Halternate-screen\\n\\033[32mgreen\\033[0m\\n" ;;' \
        '    main) printf "\\033[?1049l" ;;' \
        '    cursor) printf "\\033[?25lhidden\\033[?25h\\033[2 q\\n" ;;' \
        '    quit) exit 0 ;;' \
        '  esac' \
        'done' >"$script"
    chmod 700 "$script"
    env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET \
        RIO_CONFIG_HOME="$config" RIO_LOG_LEVEL=INFO RIO_ACCEPT_ACK_FILE="$ack" \
        DISPLAY="$DISPLAY" XDG_RUNTIME_DIR="$RUNTIME_DIR" TMPDIR="$TMPDIR_RUN" \
        "$binary" --enable-log-file --title-placeholder "visual-$variant" \
        --app-id "visual-$variant" -e /bin/sh "$script" >"$stdout" 2>&1 &
    local root=$!
    PIDS+=("$root")
    wait_until "$variant shell" 30 "[[ -f \"$ack\" ]] && grep -Fq READY \"$ack\""
    local window
    window=$(wait_for_window "$root")
    xdotool windowmove "$window" "$x" 0 2>/dev/null || true
    xdotool windowsize "$window" 800 490 2>/dev/null || true
    printf '%s\n' "$root" >"$RUN_DIR/$variant.pid"
    printf '%s\n' "$window" >"$RUN_DIR/$variant.window"
}

capture_state_set() {
    local variant=$1 window x
    window=$(<"$RUN_DIR/$variant.window")
    x=$([[ "$variant" == current ]] && printf '0' || printf '800')
    xprop -id "$window" _NET_WM_PID >/dev/null 2>&1 || die "$variant window disappeared"
    sleep 0.5
    capture "$window" "$variant-base"

    send_line "$window" alt
    sleep 0.3
    capture "$window" "$variant-alt"
    state_delta "$variant-base" "$variant-alt" "$variant.alternate_screen_delta"

    send_line "$window" main
    sleep 0.3
    capture "$window" "$variant-main"

    xdotool windowactivate --sync "$window" 2>/dev/null || true
    xdotool key --clearmodifiers ctrl+shift+o
    sleep 0.5
    capture "$window" "$variant-hints"
    state_delta "$variant-main" "$variant-hints" "$variant.hints_delta"
    xdotool key --clearmodifiers Escape

    send_line "$window" cursor
    sleep 0.3
    capture "$window" "$variant-cursor"
    state_delta "$variant-main" "$variant-cursor" "$variant.cursor_delta"

    # Use a public configured SelectAll binding instead of relying on pointer
    # coordinates, which vary with window decorations.
    capture "$window" "$variant-selection-before"
    xdotool windowactivate --sync "$window" 2>/dev/null || true
    xdotool key --clearmodifiers ctrl+shift+a
    sleep 1
    capture "$window" "$variant-selection"
    state_delta "$variant-selection-before" "$variant-selection" "$variant.selection_delta"
}

require_tools
mkdir -p "$RUN_DIR" "$RUNTIME_DIR" "$TMPDIR_RUN" "$HOME_DIR" "$CONFIG_DIR"
chmod 700 "$RUN_DIR" "$RUNTIME_DIR" "$TMPDIR_RUN" "$HOME_DIR" "$CONFIG_DIR"
log "run_dir=$RUN_DIR"
log "display=$DISPLAY resolution=1600x900 state=CPU/Xvfb"
Xvfb "$DISPLAY" -screen 0 1600x900x24 -nolisten tcp >"$RUN_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
PIDS+=("$XVFB_PID")
wait_until "private Xvfb" 20 "DISPLAY=\"$DISPLAY\" xdpyinfo >/dev/null 2>&1"
HOME="$HOME_DIR" XDG_CONFIG_HOME="$RUN_DIR/xdg-config" DISPLAY="$DISPLAY" \
    openbox --sm-disable >"$RUN_DIR/openbox.log" 2>&1 &
OPENBOX_PID=$!
PIDS+=("$OPENBOX_PID")
sleep 0.5

start_variant current "$RIO_CURRENT_BIN" 0
start_variant upstream "$RIO_UPSTREAM_BIN" 800
capture_state_set current
capture_state_set upstream

for state in base main alt hints cursor selection; do
    current="$RUN_DIR/current-$state.png"
    upstream="$RUN_DIR/upstream-$state.png"
    metric=$(compare -metric AE "$current" "$upstream" null: 2>&1 || true)
    printf 'cross_variant.%s=%s\n' "$state" "$metric" >>"$RUN_DIR/summary.log"
done

printf '%s\n' \
    'ime=UNVERIFIED: no native IME injector in the private fixture' \
    'kitty_sixel=UNVERIFIED: no protocol fixture was claimed by this run' \
    'filters=UNVERIFIED: CPU/Xvfb intentionally exercises no GPU filter path' \
    'note=state deltas and cross-variant pixel metrics are evidence, not a complete parity claim' \
    >>"$RUN_DIR/summary.log"
log "PASS: deterministic direct-render visual fixture captured colors/underline/cursor/alternate/selection/hints"
log "UNVERIFIED: IME, Kitty/Sixel, and GPU filter visual parity were not claimed"
