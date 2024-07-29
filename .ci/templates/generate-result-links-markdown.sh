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
ensure_file_exists "dsd_run_id"
ensure_file_exists "dsd_job_start_time"

adp_run_id=$(cat adp_run_id)
adp_job_start_time=$(cat adp_job_start_time)
dsd_run_id=$(cat dsd_run_id)
dsd_job_start_time=$(cat dsd_job_start_time)

# Load the job start times and figure out which job started first, which we'll use at the start anchor for the time
# range in dashboards.
if [ "$adp_job_start_time" -lt "$dsd_job_start_time" ]; then
    start_time=$adp_job_start_time
else
    start_time=$dsd_job_start_time
fi

end_time=$(cat report_end_time)

# Grab the experiments for both DSD and ADP, which may or may not overlap.
find test/smp/regression/saluki/cases -mindepth 1 -maxdepth 1 -type d | sed s#test/smp/regression/saluki/cases/##g | sort | uniq > adp-experiments
find test/smp/regression/dogstatsd/cases -mindepth 1 -maxdepth 1 -type d | sed s#test/smp/regression/dogstatsd/cases/##g | sort | uniq > dsd-experiments

adp_only_experiments=$(comm -23 adp-experiments dsd-experiments)
dsd_only_experiments=$(comm -13 adp-experiments dsd-experiments)
common_experiments=$(comm -12 adp-experiments dsd-experiments)

# Write out our table of links, doing common experiments first, then ADP-only, then DSD-only.
echo "## Experiment Result Links"
echo ""
echo "| experiment | links |"
echo "|------------|-------|"

for experiment in $common_experiments; do
    adp_continuous_profiler_url=$(get_continuous_profiler_url "$adp_run_id" "$start_time" "$end_time" "$experiment")
    dsd_continuous_profiler_url=$(get_continuous_profiler_url "$dsd_run_id" "$start_time" "$end_time" "$experiment")
    adp_smp_dashboard_url=$(get_adp_smp_dashboard_url "$adp_run_id" "$dsd_run_id" "$start_time" "$end_time" "$experiment")

    echo "| $experiment | \\[[Continuous Profiler (ADP)]($adp_continuous_profiler_url)\\] \\[[Continuous Profiler (DSD)]($dsd_continuous_profiler_url)\\] \\[[SMP Dashboard]($adp_smp_dashboard_url)\\] |"
done

for experiment in $adp_only_experiments; do
    adp_continuous_profiler_url=$(get_continuous_profiler_url "$adp_run_id" "$start_time" "$end_time" "$experiment")
    adp_smp_dashboard_url=$(get_adp_smp_dashboard_url "$adp_run_id" "$dsd_run_id" "$start_time" "$end_time" "$experiment")

    echo "| $experiment (ADP only) | \\[[Continuous Profiler (ADP)]($adp_continuous_profiler_url)\\] \\[[SMP Dashboard]($adp_smp_dashboard_url)\\] |"
done

for experiment in $dsd_only_experiments; do
    dsd_continuous_profiler_url=$(get_continuous_profiler_url "$dsd_run_id" "$start_time" "$end_time" "$experiment")
    adp_smp_dashboard_url=$(get_adp_smp_dashboard_url "$adp_run_id" "$dsd_run_id" "$start_time" "$end_time" "$experiment")

    echo "| $experiment (DSD only) | \\[[Continuous Profiler (DSD)]($dsd_continuous_profiler_url)\\] \\[[SMP Dashboard]($adp_smp_dashboard_url)\\] |"
done
