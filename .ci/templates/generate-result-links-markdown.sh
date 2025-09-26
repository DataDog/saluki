#!/usr/bin/env bash

set -euo pipefail

ensure_file_exists() {
    local file_path=$1

    if [ ! -f "$file_path" ]; then
        echo "File $file_path does not exist."
        exit 1
    fi
}

get_continuous_profiler_url() {
    local run_id=$1
    local start_time=$2
    local end_time=$3
    local experiment=$4

    query=$(printf %s "env:single-machine-performance run-id:${run_id} experiment:${experiment}" | jq -sRr @uri)

    echo "https://app.datadoghq.com/profiling/explorer?query=${query}&fromUser=true&my_code=enabled&refresh_mode=paused&viz=flame_graph&from_ts=${start_time}000&to_ts=${end_time}000&live=false"
}

get_adp_smp_dashboard_url() {
    local run_id=$1
    local start_time=$2
    local end_time=$3
    local experiment=$4

    echo "https://app.datadoghq.com/dashboard/4br-nxz-khi?fromUser=true&tpl_var_adp-run-id%5B0%5D=${adp_run_id}&tpl_var_experiment%5B0%5D=${experiment}&view=spans&from_ts=${start_time}000&to_ts=${end_time}000&live=false"
}

get_checks_smp_dashboard_url() {
    local checks_run_id=$1
    local checks_go_run_id=$2
    local start_time=$3
    local end_time=$4
    local experiment=$5

    echo "https://app.datadoghq.com/dashboard/mdp-8ua-qj3?fromUser=true&tpl_var_check-agent-rust-run-id%5B0%5D=${checks_run_id}&tpl_var_experiment%5B0%5D=${experiment}&tpl_var_checks-agent-go-run-id%5B0%5D=${checks_go_run_id}&view=spans&from_ts=${start_time}000&to_ts=${end_time}000&live=false"
}

# Make sure all required files are present.
#
# These are generate by the individual benchmark jobs and should be pulled in by the job that runs this script, and all
# of them are required to properly generate our output.
ensure_file_exists "adp_run_id"
ensure_file_exists "adp_job_start_time"
ensure_file_exists "adp_job_end_time"
ensure_file_exists "adp_checks_run_id"
ensure_file_exists "adp_checks_job_start_time"
ensure_file_exists "adp_checks_job_end_time"

adp_run_id=$(cat adp_run_id)
adp_start_time=$(cat adp_job_start_time)
adp_end_time=$(cat adp_job_end_time)
adp_checks_run_id=$(cat adp_checks_run_id)
adp_checks_start_time=$(cat adp_checks_job_start_time)
adp_checks_end_time=$(cat adp_checks_job_end_time)

# Grab the overall list of experiments.
adp_experiments=$(find test/smp/regression/saluki/cases -mindepth 1 -maxdepth 1 -type d | sed s#test/smp/regression/saluki/cases/##g | sort | uniq)
adp_checks_agent_experiments=$(find test/smp/regression/adp-checks-agent/cases -mindepth 1 -maxdepth 1 -type d | sed s#test/smp/regression/adp-checks-agent/cases/##g | sort | uniq)

# Write out our table of links.
echo "## ADP Experiment Result Links"
echo ""
echo "| experiment | link(s) |"
echo "|------------|---------|"

for experiment in $adp_experiments; do
    continuous_profiler_url=$(get_continuous_profiler_url "$adp_run_id" "$adp_start_time" "$adp_end_time" "$experiment")
    smp_dashboard_url=$(get_adp_smp_dashboard_url "$adp_run_id" "$adp_start_time" "$adp_end_time" "$experiment")

    echo "| $experiment | \\[[Profiling]($continuous_profiler_url)\\] \\[[SMP Dashboard]($smp_dashboard_url)\\] |"
done

echo "## ADP && Checks Experiment Result Links"
echo ""
echo "| experiment | link(s) |"
echo "|------------|---------|"

for experiment in $adp_checks_agent_experiments; do
    continuous_profiler_url=$(get_continuous_profiler_url "$adp_checks_run_id" "$adp_checks_start_time" "$adp_checks_end_time" "$experiment")
    smp_dashboard_url=$(get_checks_smp_dashboard_url "$adp_checks_run_id" "non-existent" "$adp_checks_start_time" "$adp_checks_end_time" "$experiment")

    echo "| $experiment | \\[[Profiling]($continuous_profiler_url)\\] \\[[SMP Dashboard]($smp_dashboard_url)\\] |"
done
