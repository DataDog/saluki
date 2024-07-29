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
    local adp_run_id=$1
    local dsd_run_id=$2
    local start_time=$3
    local end_time=$4
    local experiment=$5

    echo "https://app.datadoghq.com/dashboard/4br-nxz-khi?fromUser=true&tpl_var_dsd-run-id%5B0%5D=${dsd_run_id}&tpl_var_experiment%5B0%5D=${experiment}&tpl_var_adp-run-id%5B0%5D=${adp_run_id}&view=spans&from_ts=${start_time}000&to_ts=${end_time}000&live=false"
}

# Make sure all required files are present.
#
# These are generate by the individual benchmark jobs and should be pulled in by the job that runs this script, and all
# of them are required to properly generate our output.
ensure_file_exists "adp_run_id"
ensure_file_exists "adp_job_start_time"
ensure_file_exists "adp_job_end_time"
ensure_file_exists "dsd_run_id"
ensure_file_exists "dsd_job_start_time"
ensure_file_exists "dsd_job_end_time"

adp_run_id=$(cat adp_run_id)
adp_start_time=$(cat adp_job_start_time)
adp_end_time=$(cat adp_job_end_time)
dsd_run_id=$(cat dsd_run_id)
dsd_start_time=$(cat dsd_job_start_time)
dsd_end_time=$(cat dsd_job_end_time)

# Load the job start/end times and figure out which job started first and which job ended last, which we'll use as the
# start/end time for our dashboard, which shows both sides -- ADP and DSD -- in the same pane of glass.
if [ "$adp_start_time" -lt "$dsd_start_time" ]; then
    common_start_time=$adp_start_time
else
    common_start_time=$dsd_start_time
fi

if [ "$adp_end_time" -lt "$dsd_end_time" ]; then
    common_end_time=$dsd_end_time
else
    common_end_time=$adp_end_time
fi

# Grab the experiments for both DSD and ADP, which may or may not overlap.
find test/smp/regression/saluki/cases -mindepth 1 -maxdepth 1 -type d | sed s#test/smp/regression/saluki/cases/##g | sort | uniq > adp-experiments
find test/smp/regression/dogstatsd/cases -mindepth 1 -maxdepth 1 -type d | sed s#test/smp/regression/dogstatsd/cases/##g | sort | uniq > dsd-experiments

adp_only_experiments=$(comm -23 adp-experiments dsd-experiments)
dsd_only_experiments=$(comm -13 adp-experiments dsd-experiments)
common_experiments=$(comm -12 adp-experiments dsd-experiments)

# Write out our table of links, doing common experiments first, then ADP-only, then DSD-only.
echo "## Experiment Result Links"
echo ""
echo "| experiment | link(s) |"
echo "|------------|-------|"

for experiment in $common_experiments; do
    adp_continuous_profiler_url=$(get_continuous_profiler_url "$adp_run_id" "$adp_start_time" "$adp_end_time" "$experiment")
    dsd_continuous_profiler_url=$(get_continuous_profiler_url "$dsd_run_id" "$dsd_start_time" "$dsd_end_time" "$experiment")
    adp_smp_dashboard_url=$(get_adp_smp_dashboard_url "$adp_run_id" "$dsd_run_id" "$common_start_time" "$common_end_time" "$experiment")

    echo "| $experiment | \\[[Profiling (ADP)]($adp_continuous_profiler_url)\\] \\[[Profiling (DSD)]($dsd_continuous_profiler_url)\\] \\[[SMP Dashboard]($adp_smp_dashboard_url)\\] |"
done

for experiment in $adp_only_experiments; do
    adp_continuous_profiler_url=$(get_continuous_profiler_url "$adp_run_id" "$adp_start_time" "$adp_end_time" "$experiment")
    adp_smp_dashboard_url=$(get_adp_smp_dashboard_url "$adp_run_id" "$dsd_run_id" "$adp_start_time" "$adp_end_time" "$experiment")

    echo "| $experiment (ADP only) | \\[[Profiling (ADP)]($adp_continuous_profiler_url)\\] \\[[SMP Dashboard]($adp_smp_dashboard_url)\\] |"
done

for experiment in $dsd_only_experiments; do
    dsd_continuous_profiler_url=$(get_continuous_profiler_url "$dsd_run_id" "$dsd_start_time" "$dsd_end_time" "$experiment")
    adp_smp_dashboard_url=$(get_adp_smp_dashboard_url "$adp_run_id" "$dsd_run_id" "$dsd_start_time" "$dsd_end_time" "$experiment")

    echo "| $experiment (DSD only) | \\[[Profiling (DSD)]($dsd_continuous_profiler_url)\\] \\[[SMP Dashboard]($adp_smp_dashboard_url)\\] |"
done
