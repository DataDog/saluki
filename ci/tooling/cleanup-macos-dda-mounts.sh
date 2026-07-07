#!/usr/bin/env bash
# Cleans up temporary Datadog Agent DMG mount points left by macOS integration-test setup.
set -euo pipefail

retry_detach() {
  local mount_dir="$1"

  if [[ ! -d "$mount_dir" ]]; then
    return 0
  fi

  if ! mount | grep -F " on ${mount_dir} " >/dev/null 2>&1 && ! hdiutil info | grep -F "$mount_dir" >/dev/null 2>&1; then
    rmdir "$mount_dir" 2>/dev/null || true
    return 0
  fi

  local attempt
  for attempt in 1 2 3; do
    if hdiutil detach "$mount_dir" >/dev/null 2>&1; then
      rmdir "$mount_dir" 2>/dev/null || true
      return 0
    fi

    echo "[*] Failed to detach ${mount_dir} on attempt ${attempt}; retrying..." >&2
    sleep $((attempt * 2))
  done

  echo "[*] Forcing detach of ${mount_dir} after retries failed..." >&2
  hdiutil detach -force "$mount_dir" >/dev/null 2>&1 || true
  rmdir "$mount_dir" 2>/dev/null || true
}

if [[ "$#" -gt 0 ]]; then
  for mount_dir in "$@"; do
    retry_detach "$mount_dir"
  done
else
  shopt -s nullglob
  for mount_dir in /tmp/saluki-dda-mount-*; do
    retry_detach "$mount_dir"
  done
fi
