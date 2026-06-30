#!/usr/bin/env sh
#
# Runs all step scripts for a given project / stage.
#
# Step scripts are any scripts that match the form of `<number>-*.sh`. Step scripts are run in
# lexicographical order, lowest to highest.
#
# Usage: run-stage.sh <project> <stage>
set -eu

# Find all numbered step scripts in the current directory and run them in order, lowest to highest.
script_dir="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"

project="$1"
stage="$2"
stage_dir="${script_dir}/${project}/${stage}"

if [ ! -d "${stage_dir}" ]; then
    echo "Stage directory not found: ${stage_dir}"
    exit 1
fi

echo "[*] Executing '${project}/${stage}'..."

for script in "${stage_dir}"/*; do
    if [ -f "${script}" ]; then
        stripped_script="$(basename "${script}")"
        echo "[*] Running step: ${stripped_script}..."
        "${script}"
    fi
done
