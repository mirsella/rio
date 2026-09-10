#!/usr/bin/env bash
set -Eeuo pipefail
source "$(dirname -- "${BASH_SOURCE[0]}")/lib/session-isolation-common.sh"

# Compare the production compositor path with upstream/main on one private
# X11 display. This intentionally measures process/PTY behavior, not GPU
# presentation latency; Xvfb is used only to keep input and capture isolated.

ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
ARTIFACT_ROOT=${RIO_BENCH_ARTIFACT_ROOT:-"$HOME/dev/rio-agent-artifacts"}
RUN_DIR="$ARTIFACT_ROOT/direct-benchmark-$(date -u +%Y%m%dT%H%M%SZ)-$$"
DISPLAY_NUM=${RIO_BENCH_DISPLAY_NUM:-$((100 + ($$ % 700)))}
export DISPLAY=":$DISPLAY_NUM"
CURRENT_BIN=${RIO_CURRENT_BIN:-"$ROOT/target/debug/rio"}
UPSTREAM_BIN=${RIO_UPSTREAM_BIN:-"$ARTIFACT_ROOT/target-upstream-direct-bench/debug/rio"}
WINDOW_TIMEOUT=${RIO_BENCH_WINDOW_TIMEOUT:-45}
ACK_SAMPLES=${RIO_BENCH_ACK_SAMPLES:-5}
ACK_POLL_SECONDS=0.1
METRIC_SAMPLE_SECONDS=0.25
IDLE_SECONDS=2.0
SCROLL_SECONDS=3.5
CURSOR_SECONDS=2.5
SCROLL_LINES=600
CURSOR_UPDATES=250
WORKLOAD_BATCH=10
WORKLOAD_PAUSE_SECONDS=0.01
XVFB=${XVFB:-/usr/bin/Xvfb}
XDPYINFO=${XDPYINFO:-/usr/bin/xdpyinfo}
XDOTTOOL=${XDOTTOOL:-/usr/bin/xdotool}
XPROP=${XPROP:-/usr/bin/xprop}
OPENBOX=${OPENBOX:-/usr/bin/openbox}

mkdir -p "$RUN_DIR"
chmod 700 "$RUN_DIR"
unset WAYLAND_DISPLAY WAYLAND_SOCKET

for command in "$XVFB" "$XDPYINFO" "$XDOTTOOL" "$XPROP" "$OPENBOX" ps pgrep awk date getconf; do
    require_command "$command"
done
CLK_TCK=$(getconf CLK_TCK)
[[ -x "$CURRENT_BIN" ]] || die "current binary is not executable: $CURRENT_BIN"
[[ -x "$UPSTREAM_BIN" ]] || die "upstream binary is not executable: $UPSTREAM_BIN"

PIDS=()
XVFB_PID=
trap cleanup EXIT INT TERM

wait_for_marker() {
    local file=$1 marker=$2 timeout=${3:-15}
    wait_for_text "$file" "$marker" "'$marker' in $file" "$timeout"
}

wait_for_window() {
    local root=$1 result deadline=$((SECONDS + WINDOW_TIMEOUT))
    while ((SECONDS < deadline)); do
        result=$(window_for_pid "$root" 2>/dev/null || true)
        if [[ "$result" =~ ^[0-9]+$ ]]; then
            WINDOW_RESULT=$result
            return 0
        fi
        sleep "$ACK_POLL_SECONDS"
    done
    local name
    while read -r window; do
        [[ -n "$window" ]] || continue
        name=$("$XDOTTOOL" getwindowname "$window" 2>/dev/null || true)
        log "window candidate id=$window name=$name"
    done < <("$XDOTTOOL" search --onlyvisible --name '.*' 2>/dev/null || true)
    log "rio process at window timeout: $(ps -p "$root" -o pid=,ppid=,stat=,args= 2>/dev/null || true)"
    return 1
}

track_runtime_processes() {
    local runtime=$1 pid
    while read -r pid; do
        [[ "$pid" =~ ^[0-9]+$ ]] || continue
        PIDS+=("$pid")
    done < <(pgrep -f -- "$runtime/rio-session-" 2>/dev/null || true)
}

send_line() {
    local window=$1 line=$2
    local index key
    "$XDOTTOOL" windowactivate --sync "$window" 2>/dev/null || true
    "$XDOTTOOL" windowfocus --sync "$window" 2>/dev/null || true
    for ((index = 0; index < ${#line}; index++)); do
        key=${line:index:1}
        [[ "$key" == ' ' ]] && key=space
        [[ "$key" == '-' ]] && key=minus
        "$XDOTTOOL" key --clearmodifiers "$key"
    done
    "$XDOTTOOL" key Return
}

measure_processes() {
    local variant=$1 phase=$2 root=$3 seconds=$4
    local elapsed=0.0 now cpu rss count start_ns previous_ns previous_ticks current_ticks
    start_ns=$(date +%s%N)
    previous_ns=$(date +%s%N)
    mapfile -t tree < <(printf '%s\n' "$root"; owned_descendants "$root")
    previous_ticks=0
    for pid in "${tree[@]}"; do
        [[ "$pid" =~ ^[0-9]+$ ]] || continue
        ticks=$(awk '{print $14 + $15}' "/proc/$pid/stat" 2>/dev/null || true)
        [[ "$ticks" =~ ^[0-9]+$ ]] && previous_ticks=$((previous_ticks + ticks))
    done
    while awk -v elapsed="$elapsed" -v limit="$seconds" 'BEGIN {exit !(elapsed < limit)}'; do
        sleep "$METRIC_SAMPLE_SECONDS"
        now=$(date +%s%N)
        mapfile -t tree < <(printf '%s\n' "$root"; owned_descendants "$root")
        current_ticks=0
        rss=0
        count=0
        for pid in "${tree[@]}"; do
            [[ "$pid" =~ ^[0-9]+$ ]] || continue
            ticks=$(awk '{print $14 + $15}' "/proc/$pid/stat" 2>/dev/null || true)
            [[ "$ticks" =~ ^[0-9]+$ ]] && current_ticks=$((current_ticks + ticks))
            read -r process_cpu process_rss < <(ps -p "$pid" -o %cpu=,rss= 2>/dev/null || true)
            [[ "$process_rss" =~ ^[0-9]+$ ]] || continue
            rss=$((rss + process_rss))
            count=$((count + 1))
        done
        cpu=$(awk -v ticks="$((current_ticks - previous_ticks))" \
            -v elapsed_ns="$((now - previous_ns))" -v hz="$CLK_TCK" \
            'BEGIN {printf "%.2f", ticks / hz / (elapsed_ns / 1000000000) * 100}')
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$variant" "$phase" "$now" "$cpu" "$rss" "$count" "$root" >>"$RUN_DIR/metrics.tsv"
        previous_ticks=$current_ticks
        previous_ns=$now
        elapsed=$(awk -v start="$start_ns" -v end="$now" 'BEGIN {printf "%.3f", (end-start)/1000000000}')
    done
}

write_config() {
    local config=$1
    mkdir -p "$config/log"
    chmod 700 "$config"
    printf '%s\n' \
        'draw-bold-text-with-light-colors = true' \
        '' \
        '[renderer]' \
        'use-cpu = true' \
        '' \
        '[developer]' \
        'log-level = "INFO"' >"$config/config.toml"
}

run_variant() {
    local variant=$1 binary=$2
    local variant_dir="$RUN_DIR/$variant"
    local config="$variant_dir/config"
    local runtime tmpdir
    if [[ "$variant" == current ]]; then
        runtime="$ARTIFACT_ROOT/r-$$"
        tmpdir="$ARTIFACT_ROOT/t-$$"
    else
        runtime="$ARTIFACT_ROOT/u-$$"
        tmpdir="$ARTIFACT_ROOT/v-$$"
    fi
    local ack="$variant_dir/ack.log"
    local stdout="$variant_dir/stdout.log"
    local root window sample before after id bytes
    mkdir -p "$variant_dir" "$runtime" "$tmpdir"
    chmod 700 "$variant_dir" "$runtime" "$tmpdir"
    write_config "$config"

    log "benchmark variant=$variant binary=$binary"
    env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET DISPLAY="$DISPLAY" \
        XDG_RUNTIME_DIR="$runtime" TMPDIR="$tmpdir" RIO_CONFIG_HOME="$config" RIO_LOG_LEVEL=INFO \
        RIO_BENCH_SCROLL_LINES="$SCROLL_LINES" RIO_BENCH_CURSOR_UPDATES="$CURSOR_UPDATES" \
        RIO_BENCH_WORKLOAD_BATCH="$WORKLOAD_BATCH" \
        RIO_BENCH_WORKLOAD_PAUSE_SECONDS="$WORKLOAD_PAUSE_SECONDS" \
        RIO_BENCH_ACK_FILE="$ack" "$binary" --enable-log-file \
        --title-placeholder "rio-bench-$variant" --app-id "rio-bench-$variant" \
        -e bash -lc '
            printf "READY pid=%s\\n" "$$" >"$RIO_BENCH_ACK_FILE"
            ack_count=0
            pty_bytes=0
            while IFS= read -r line; do
                case "$line" in
                    ack\ *)
                        ack_count=$((ack_count + 1))
                        printf "ACK id=%s pid=%s count=%s\\n" "${line#ack }" "$$" "$ack_count" >>"$RIO_BENCH_ACK_FILE"
                        ;;
                    scroll)
                        for ((i = 1; i <= RIO_BENCH_SCROLL_LINES; i++)); do
                            text=$(printf "scroll-%06d direct-frame workload\\n" "$i")
                            printf "%s" "$text"
                            pty_bytes=$((pty_bytes + ${#text}))
                            ((i % RIO_BENCH_WORKLOAD_BATCH == 0)) && sleep "$RIO_BENCH_WORKLOAD_PAUSE_SECONDS"
                        done
                        printf "SCROLL_DONE bytes=%s\\n" "$pty_bytes" >>"$RIO_BENCH_ACK_FILE"
                        ;;
                    cursor)
                        sequence=$(printf "\\033[?25l\\033[?25h\\033[2 q")
                        for ((i = 1; i <= RIO_BENCH_CURSOR_UPDATES; i++)); do
                            printf "%s" "$sequence"
                            pty_bytes=$((pty_bytes + ${#sequence}))
                            ((i % RIO_BENCH_WORKLOAD_BATCH == 0)) && sleep "$RIO_BENCH_WORKLOAD_PAUSE_SECONDS"
                        done
                        printf "CURSOR_DONE bytes=%s\\n" "$pty_bytes" >>"$RIO_BENCH_ACK_FILE"
                        ;;
                esac
            done
        ' >"$stdout" 2>&1 &
    root=$!
    PIDS+=("$root")
    wait_for_marker "$ack" READY
    if ! wait_for_window "$root"; then
        die "timed out waiting for private window for pid $root"
    fi
    window=$WINDOW_RESULT
    track_runtime_processes "$runtime"
    if [[ "$variant" == current ]]; then
        mkdir -p "$RUN_DIR/home" "$RUN_DIR/xdg-config"
        HOME="$RUN_DIR/home" XDG_CONFIG_HOME="$RUN_DIR/xdg-config" DISPLAY="$DISPLAY" \
            "$OPENBOX" --sm-disable >"$RUN_DIR/openbox.log" 2>&1 &
        OPENBOX_PID=$!
        PIDS+=("$OPENBOX_PID")
        sleep 0.5
    fi
    "$XDOTTOOL" windowsize "$window" 800 490 2>/dev/null || true
    sleep 0.5

    for ((sample = 1; sample <= ACK_SAMPLES; sample++)); do
        id="${variant}-ack-${sample}"
        before=$(date +%s%N)
        send_line "$window" "ack $id"
        wait_for_marker "$ack" "ACK id=$id" 10
        after=$(date +%s%N)
        printf '%s\tack_latency_ms\t%s\n' "$variant" "$(( (after - before) / 1000000 ))" >>"$RUN_DIR/latency.tsv"
    done

    measure_processes "$variant" idle "$root" "$IDLE_SECONDS" &
    local sampler=$!
    wait "$sampler"

    measure_processes "$variant" scroll "$root" "$SCROLL_SECONDS" &
    sampler=$!
    send_line "$window" scroll
    wait_for_marker "$ack" SCROLL_DONE 20
    wait "$sampler"
    bytes=$(awk -F'bytes=' '/SCROLL_DONE/ {print $2; exit}' "$ack")
    printf '%s\tpty_output_bytes\t%s\n' "$variant" "$bytes" >>"$RUN_DIR/workload.tsv"

    measure_processes "$variant" cursor "$root" "$CURSOR_SECONDS" &
    sampler=$!
    send_line "$window" cursor
    wait_for_marker "$ack" CURSOR_DONE 20
    wait "$sampler"
    bytes=$(awk -F'bytes=' '/CURSOR_DONE/ {print $2; exit}' "$ack")
    printf '%s\tcursor_output_bytes\t%s\n' "$variant" "$bytes" >>"$RUN_DIR/workload.tsv"

    # Stop before the next binary starts so both variants own the same display
    # and resolution without overlap or inherited desktop interaction.
    stop_tree "$root"
    log "benchmark variant=$variant complete"
}

log "run_dir=$RUN_DIR"
log "display=$DISPLAY resolution=800x490 backend=CPU/Xvfb"
log "current_binary=$CURRENT_BIN"
log "upstream_binary=$UPSTREAM_BIN"
log "ack_samples=$ACK_SAMPLES marker_poll_seconds=$ACK_POLL_SECONDS input=per-character-xdotool-no-fixed-interkey-delay"
log "cpu_rss_sample_seconds=$METRIC_SAMPLE_SECONDS phases=idle:${IDLE_SECONDS}s,scroll:${SCROLL_SECONDS}s,cursor:${CURSOR_SECONDS}s"
log "workload=scroll:${SCROLL_LINES}-lines,cursor:${CURSOR_UPDATES}-updates,batch:${WORKLOAD_BATCH},pause:${WORKLOAD_PAUSE_SECONDS}s"
"$XVFB" "$DISPLAY" -screen 0 1600x1000x24 -nolisten tcp >"$RUN_DIR/xvfb.log" 2>&1 &
XVFB_PID=$!
PIDS+=("$XVFB_PID")
wait_until "private Xvfb" 15 "kill -0 $XVFB_PID 2>/dev/null && DISPLAY=\"$DISPLAY\" $XDPYINFO >/dev/null 2>&1"

printf 'variant\tphase\tnanoseconds\tinterval_cpu_percent\ttotal_rss_kib\tprocess_count\troot_pid\n' >"$RUN_DIR/metrics.tsv"
: >"$RUN_DIR/latency.tsv"
: >"$RUN_DIR/workload.tsv"
run_variant current "$CURRENT_BIN"
run_variant upstream "$UPSTREAM_BIN"

log "benchmark complete; metrics=$RUN_DIR/metrics.tsv latency=$RUN_DIR/latency.tsv workload=$RUN_DIR/workload.tsv"
log "PTY bytes are workload accounting only; they are not frame or present latency measurements"
log "PASS: bounded private direct-render comparison completed"
