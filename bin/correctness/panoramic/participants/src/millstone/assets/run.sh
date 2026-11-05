#!/bin/sh
set -u

binary=$1
config_dir=$2
state_dir=$3
: "${PANORAMIC_EXIT_DIR:?}"
: "${PANORAMIC_LOG_DIR:?}"
mkdir -p "$state_dir" "$PANORAMIC_EXIT_DIR" "$PANORAMIC_LOG_DIR" || exit 1

run_sender() {
    process=$1
    record_exit() {
        status=$1
        code=$status
        signal=0
        # Millstone exits with 0 or 1; larger wait statuses identify signal deaths.
        if [ "$status" -gt 128 ]; then
            code=256
            signal=$((status - 128))
        fi
        printf '%s %s %s.000000000\n' "$code" "$signal" "$(date +%s)" >> "$PANORAMIC_EXIT_DIR/$process"
    }
    stop_sender() {
        trap '' TERM INT
        kill -TERM "$child" 2>/dev/null
        wait "$child"
        record_exit "$?"
        exit 0
    }

    "$binary" "$config_dir/$process.yaml" \
        --completion-file "$state_dir/$process.complete" \
        --traffic-capture-dir "$state_dir/$process" &
    child=$!
    trap stop_sender TERM INT
    wait "$child"
    status=$?
    record_exit "$status"
    exit "$status"
}

# Each monitor records its own exit even while the other sender stays alive.
run_sender baseline > "$PANORAMIC_LOG_DIR/baseline.log" 2>&1 &
baseline=$!
run_sender subject > "$PANORAMIC_LOG_DIR/subject.log" 2>&1 &
subject=$!
trap 'trap "" TERM INT; kill -TERM "$baseline" "$subject" 2>/dev/null; wait "$baseline"; wait "$subject"; exit 0' TERM INT
wait "$baseline"
baseline_status=$?
wait "$subject"
subject_status=$?
exit $((baseline_status | subject_status))
