# Shared lifecycle helpers for the private acceptance harnesses.
# This file is sourced by scripts in the parent directory.

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
    local root=$1 pid index
    local -a tree=()
    [[ "$root" =~ ^[0-9]+$ ]] || return 0
    mapfile -t tree < <(printf '%s\n' "$root"; owned_descendants "$root")
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
    local root
    local -a roots=()
    [[ ${PIDS+x} ]] && roots+=("${PIDS[@]}")
    [[ ${ORPHANS+x} ]] && roots+=("${ORPHANS[@]}")
    [[ ${XVFB_PID+x} ]] && roots+=("$XVFB_PID")
    [[ ${OPENBOX_PID+x} ]] && roots+=("$OPENBOX_PID")
    [[ ${DBUS_PID+x} ]] && roots+=("$DBUS_PID")
    [[ ${CLIPBOARD_PID+x} ]] && roots+=("$CLIPBOARD_PID")
    roots+=("$@")
    for root in "${roots[@]}"; do
        stop_tree "$root"
    done
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
