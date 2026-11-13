#!/bin/sh

# Runs millstone's two senders side by side: each writes its output to the log file the harness follows, and appends
# its exit to its exit record. A sender started with `--completion-file` stays alive once it has sent everything, so
# the Agent and ADP can still resolve its identity while they flush.
#
# Only the baseline sender captures its input: both send byte-identical payloads from the same seed.

sender() {
    name=$1
    shift
    # Run millstone in the background so this subshell can catch a stop, pass it on, and still write the record.
    stopped=
    /usr/local/bin/millstone "$@" >>"$PANORAMIC_LOG_DIR/$name.log" 2>&1 &
    child=$!
    trap 'stopped=1; kill "$child" 2>/dev/null' TERM INT
    wait "$child"
    code=$?
    if [ -n "$stopped" ]; then
        wait "$child"
        code=$?
    fi
    signal=0
    # The shell reports a death by signal as 128 plus its number; the record carries 256 and the number.
    if [ "$code" -gt 128 ]; then
        signal=$((code - 128))
        code=256
    fi
    echo "$code $signal $(date +%s.%N)" >>"$PANORAMIC_EXIT_DIR/$name"
}

sender comparison /etc/millstone/comparison.yaml --completion-file /tmp/comparison-complete &
comparison_pid=$!
sender baseline /etc/millstone/baseline.yaml --completion-file /tmp/baseline-complete \
    --traffic-capture-dir /var/lib/millstone/baseline &
baseline_pid=$!

# On a stop, tell the two senders so each writes its exit record, then exit cleanly.
trap 'trap - TERM INT; kill "$comparison_pid" "$baseline_pid" 2>/dev/null; wait; exit 0' TERM INT
wait
