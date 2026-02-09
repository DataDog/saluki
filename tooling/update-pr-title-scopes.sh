#!/usr/bin/env bash
set -euo pipefail

# Updates (or checks) the allowed PR title scopes in the Conventional Commits
# workflow based on the actual crate and component structure of the codebase.
#
# Usage:
#   ./tooling/update-pr-title-scopes.sh update   # Update the workflow file
#   ./tooling/update-pr-title-scopes.sh check    # Check if the workflow file is up-to-date

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW_FILE="${REPO_ROOT}/.github/workflows/check-pr-title.yml"
LIB_DIR="${REPO_ROOT}/lib"
BIN_DIR="${REPO_ROOT}/bin"
COMPONENTS_SRC="${LIB_DIR}/saluki-components/src"

# Component type directories to scan. Singular form is derived by stripping the
# trailing 's'.
COMPONENT_TYPES=(decoders destinations encoders forwarders relays sources transforms)

# Fixed scopes that aren't derived from the codebase.
FIXED_SCOPES=(
    ci
    deps
    dev
    docs
)

# Reads the crate name from a Cargo.toml file.
read_crate_name() {
    grep -m1 '^name' "$1" | sed 's/^name *= *"\(.*\)"/\1/'
}

# Generates scopes from library crates under lib/.
generate_lib_scopes() {
    while IFS= read -r cargo_toml; do
        # Skip fuzz crates.
        if [[ "${cargo_toml}" == */fuzz/* ]]; then
            continue
        fi

        local crate_name
        crate_name="$(read_crate_name "${cargo_toml}")"

        # Strip saluki- prefix if present.
        echo "${crate_name#saluki-}"
    done < <(find "${LIB_DIR}" -name 'Cargo.toml' -type f | sort)
}

# Generates scopes from binary crates under bin/.
generate_bin_scopes() {
    while IFS= read -r cargo_toml; do
        read_crate_name "${cargo_toml}"
    done < <(find "${BIN_DIR}" -name 'Cargo.toml' -type f | sort)
}

# Generates scopes from component directories under saluki-components.
#
# Each immediate child directory of a component type directory is treated as a
# single component, formatted as "<name> <singular_type>" with underscores
# replaced by spaces.
generate_component_scopes() {
    for comp_type in "${COMPONENT_TYPES[@]}"; do
        local type_dir="${COMPONENTS_SRC}/${comp_type}"
        if [[ ! -d "${type_dir}" ]]; then
            continue
        fi

        # Derive singular form by stripping trailing 's'.
        local singular="${comp_type%s}"

        for comp_dir in "${type_dir}"/*/; do
            [[ -d "${comp_dir}" ]] || continue
            local comp_name
            comp_name="$(basename "${comp_dir}")"

            # Convert underscores to spaces.
            local scope_name="${comp_name//_/ }"

            echo "${scope_name} ${singular}"
        done
    done
}

# Generates the complete sorted, deduplicated list of scopes.
generate_all_scopes() {
    {
        generate_lib_scopes
        generate_bin_scopes
        generate_component_scopes
        printf '%s\n' "${FIXED_SCOPES[@]}"
    } | sort -f -u
}

# Builds the indented scopes block for the workflow file.
build_scopes_block() {
    generate_all_scopes | while IFS= read -r scope; do
        echo "            ${scope}"
    done
}

# Returns the line number of the 'scopes: |' line in the workflow file.
get_scopes_line_number() {
    grep -nE '^\s+scopes:\s*\|' "${WORKFLOW_FILE}" | head -1 | cut -d: -f1
}

# Validates that the workflow file has the expected structure.
validate_workflow_file() {
    if [[ ! -f "${WORKFLOW_FILE}" ]]; then
        echo "[!] Workflow file not found: ${WORKFLOW_FILE}"
        return 1
    fi

    if ! grep -q 'amannn/action-semantic-pull-request' "${WORKFLOW_FILE}"; then
        echo "[!] Workflow file does not contain the expected action reference (amannn/action-semantic-pull-request)."
        return 1
    fi

    if ! grep -qE '^\s+scopes:\s*\|' "${WORKFLOW_FILE}"; then
        echo "[!] Workflow file does not contain the expected 'scopes: |' key."
        return 1
    fi

    local scopes_line
    scopes_line="$(get_scopes_line_number)"

    # Determine the indentation level of the scopes key.
    local scopes_indent
    scopes_indent="$(sed -n "${scopes_line}p" "${WORKFLOW_FILE}" | sed 's/[^ ].*//' | wc -c)"

    # Verify no YAML key at same or lesser indentation appears after the scopes line,
    # which would mean scopes is not the last key (and our replacement strategy is wrong).
    if tail -n +"$((scopes_line + 1))" "${WORKFLOW_FILE}" | grep -qE "^.{0,$((scopes_indent - 2))}[^ ]"; then
        echo "[!] Found content after scopes block at same or lesser indentation level."
        echo "[!] The 'scopes' key must be the last key in the action's 'with' block."
        return 1
    fi
}

# Updates the workflow file with the generated scopes.
update_workflow_file() {
    local scopes_line
    scopes_line="$(get_scopes_line_number)"

    local new_scopes
    new_scopes="$(build_scopes_block)"

    # Keep everything up to and including the 'scopes: |' line, then append new scopes.
    local head_content
    head_content="$(head -n "${scopes_line}" "${WORKFLOW_FILE}")"

    printf '%s\n%s\n' "${head_content}" "${new_scopes}" > "${WORKFLOW_FILE}"
}

# Checks whether the workflow file scopes match the generated scopes.
check_workflow_file() {
    local scopes_line
    scopes_line="$(get_scopes_line_number)"

    local current_scopes
    current_scopes="$(tail -n +"$((scopes_line + 1))" "${WORKFLOW_FILE}")"

    local expected_scopes
    expected_scopes="$(build_scopes_block)"

    if [[ "${current_scopes}" == "${expected_scopes}" ]]; then
        echo "[*] PR title scopes are up-to-date."
        return 0
    else
        echo "[!] PR title scopes are out of date. Diff (current vs expected):"
        diff <(echo "${current_scopes}") <(echo "${expected_scopes}") || true
        echo ""
        echo "[!] Run 'make update-pr-title-scopes' to regenerate."
        return 1
    fi
}

main() {
    local mode="${1:-update}"

    validate_workflow_file || exit 1

    case "${mode}" in
        update)
            echo "[*] Updating PR title scopes..."
            update_workflow_file
            echo "[*] Done. Updated scopes in ${WORKFLOW_FILE#"${REPO_ROOT}"/}"
            ;;
        check)
            check_workflow_file
            ;;
        *)
            echo "[!] Unknown mode: ${mode}. Use 'update' or 'check'."
            exit 1
            ;;
    esac
}

main "$@"
