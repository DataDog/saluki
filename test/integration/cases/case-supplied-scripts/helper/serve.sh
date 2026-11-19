#!/bin/sh
# Starts, reports where it runs, and waits to be stopped.
trap 'exit 0' TERM INT
echo "helper started in $HELPER_DIR"
while true; do
    sleep 1
done
