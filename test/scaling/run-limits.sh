#!/usr/bin/env bash
#
# Runs ADP at production CPU limits (N1 of docs/design/programmable-metrics-next.md): for each CPU limit in
# SCALING_LIMITS and each SMP quality-gate tier in SCALING_TIERS, the two images under the same steady DogStatsD load,
# for SCALING_ROUNDS rounds each, alternating which goes first. ADP gets a fractional CPU quota on a fixed set of cores
# (`--cpus` on SCALING_ADP_CORES cores, like a container on a node) and runs with production defaults (see
# `compose.limits.yaml`), so it picks its own worker threads and decoders from the quota.
#
# Each run waits SCALING_WARMUP_SECONDS, then measures a window of SCALING_WINDOW_SECONDS: the metrics ADP decoded,
# what lading sent, and ADP's cgroup counters (CPU time, CFS periods and throttling) at both ends; every second,
# lading's bytes and ADP's cgroup counters and anonymous memory, read from the host so that the sampling costs ADP
# nothing; and every SCALING_ADP_SAMPLE_SECONDS, ADP's own telemetry (the datagram buffer pool in use). Results go to
# SCALING_OUT: runs.csv, one series-<run>.csv per run, and summary.md from summarize_limits.py.
#
# SCALING_MODE=saturate sends as fast as lading can instead of the tier's rate, to find the most each image sustains at
# each limit. SCALING_DRY_RUN=1 renders every run's lading configuration and prints the docker commands.
#
# For the quick loop of docs/design/programmable-metrics-next.md ("How we iterate"): SCALING_CELLS names the cells to
# run (`tier:limit` pairs) instead of every tier at every limit; SCALING_VARIANTS names the images to compare, each with
# its own environment (`label=image;KEY=VALUE;...`), instead of main and plan; SCALING_PARALLEL runs that many cells at
# once on disjoint cores; SCALING_PERF=1 counts instructions, cycles, and memory events in ADP's cgroup over each
# window with `perf stat`; and SCALING_RECORD=1 samples where ADP's instructions go, with call stacks, for the first
# SCALING_RECORD_SECONDS of each window (`perf record`, into perf-<run>.data). Both need `sudo -n`.

set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

limits=${SCALING_LIMITS:-"0.05 0.3 0.6 1.2"}
tiers=${SCALING_TIERS:-"low medium heavy ultraheavy"}
rounds=${SCALING_ROUNDS:-3}
main_image=${SCALING_MAIN_IMAGE:-saluki-images/agent-data-plane:main}
plan_image=${SCALING_PLAN_IMAGE:-saluki-images/agent-data-plane:plan}
aa=${SCALING_AA:-0}
mode=${SCALING_MODE:-paced}
saturate_rate=${SCALING_SATURATE_RATE:-"1 GiB"}
warmup=${SCALING_WARMUP_SECONDS:-30}
window=${SCALING_WINDOW_SECONDS:-60}
adp_sample=${SCALING_ADP_SAMPLE_SECONDS:-5}
keep_scrapes=${SCALING_KEEP_SCRAPES:-0}
cells=${SCALING_CELLS:-}
variants=${SCALING_VARIANTS:-}
parallel=${SCALING_PARALLEL:-1}
perf=${SCALING_PERF:-0}
record=${SCALING_RECORD:-0}
record_seconds=${SCALING_RECORD_SECONDS:-10}
# One cell at a time keeps N1's layout: ADP on four cores, lading on two, the blackhole on two. Cells in parallel each
# take two cores for ADP (enough for two workers at every limit up to 1.2), two for lading, and one for the blackhole.
if ((parallel > 1)); then
  adp_cores=${SCALING_ADP_CORES:-2}
  blackhole_cores=1
else
  adp_cores=${SCALING_ADP_CORES:-4}
  blackhole_cores=2
fi
slot_width=$((adp_cores + 2 + blackhole_cores))
dry_run=${SCALING_DRY_RUN:-0}
out=${SCALING_OUT:-"$here/results/limits-$(date +%Y%m%dT%H%M%S)"}

# Pinned, instructions and cycles are counted all the time; the memory events share the remaining counter and are
# scaled estimates.
perf_events=inst_retired:D,cpu_cycles:D,l1d_cache_refill,l2d_cache_refill,stall_backend,dtlb_walk

# Sets `compose` and the ports and cores of slot $1: each slot has its own project, ports, and cores.
use_slot() {
  local slot=$1 base=$(($1 * slot_width))
  compose=(docker compose --file "$here/compose.yaml" --file "$here/compose.limits.yaml"
    --project-name "saluki-limits-$$-$slot")
  export ADP_CPUSET="$base-$((base + adp_cores - 1))"
  export LADING_CPUSET="$((base + adp_cores))-$((base + adp_cores + 1))"
  if ((blackhole_cores == 1)); then
    export BLACKHOLE_CPUSET="$((base + adp_cores + 2))"
  else
    export BLACKHOLE_CPUSET="$((base + adp_cores + 2))-$((base + adp_cores + blackhole_cores + 1))"
  fi
  export ADP_TELEMETRY_PORT=$((15100 + slot * 10))
  export LADING_TELEMETRY_PORT=$((19229 + slot * 10))
  export BLACKHOLE_TELEMETRY_PORT=$((19230 + slot * 10))
}

log() {
  echo "[limits] $*" >&2
}

fail() {
  log "$*"
  exit 1
}

run() {
  if [[ "$dry_run" == 1 ]]; then
    echo "+ $*"
  else
    "$@"
  fi
}

now() {
  python3 -I -c 'import time; print(f"{time.time():.3f}")'
}

# Sets the tier's load, from the SMP case `quality_gates_rss_dsd_<tier>`: lading's rate, contexts, and block cache,
# ADP's string interner, and the case's bound on ADP's memory.
set_tier() {
  case "$1" in
    low) tier_rate="1 MiB" tier_contexts=3000 tier_cache="100 Mb" tier_interner=524288 tier_memory_bound_mib=60 ;;
    medium) tier_rate="10 MiB" tier_contexts=10000 tier_cache="500 Mb" tier_interner=524288 tier_memory_bound_mib=100 ;;
    heavy) tier_rate="50 MiB" tier_contexts=100000 tier_cache="512 Mb" tier_interner=32MiB tier_memory_bound_mib=250 ;;
    ultraheavy)
      tier_rate="100 MiB" tier_contexts=250000 tier_cache="768 Mb" tier_interner=64MiB tier_memory_bound_mib=420
      ;;
    *) fail "SCALING_TIERS takes low, medium, heavy, and ultraheavy, not '$1'" ;;
  esac
  # The heavy tiers' contexts vary by 1% around the figure, as in the SMP cases.
  tier_contexts_max=$((tier_contexts + 1))
  if ((tier_contexts >= 100000)); then
    tier_contexts_max=$((tier_contexts + tier_contexts / 100))
    tier_contexts=$((tier_contexts - tier_contexts / 100))
  fi
  if [[ "$mode" == saturate ]]; then
    tier_rate=$saturate_rate
  fi
}

# Prints a lading byte size such as `50 MiB` in bytes.
size_bytes() {
  python3 -I - "$1" <<'EOF'
import re, sys
match = re.fullmatch(r"\s*([0-9.]+)\s*([KMG]?)(i?)([Bb])\s*", sys.argv[1])
if not match:
    sys.exit(f"a size looks like '50 MiB' or '500 Mb', not {sys.argv[1]!r}")
number, prefix, binary, unit = match.groups()
power = {"": 0, "K": 1, "M": 2, "G": 3}[prefix]
print(int(float(number) * (1024 if binary else 1000) ** power / (8 if unit == "b" else 1)))
EOF
}

# Writes the lading configuration of a run to $1: the SMP quality-gate case's generator, on the shared socket.
render_lading_config() {
  cat >"$1" <<EOF
# Rendered by run-limits.sh: the SMP case quality_gates_rss_dsd_$tier ($mode, $tier_rate/s, $tier_contexts to
# $tier_contexts_max contexts).
generator:
  - unix_datagram:
      seed: [2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97, 101, 103,
        107, 109, 113, 127, 131]
      path: /var/run/dsd/dsd.sock
      block_cache_method: Fixed
      variant:
        dogstatsd:
          contexts:
            inclusive:
              min: $tier_contexts
              max: $tier_contexts_max
          name_length:
            inclusive:
              min: 1
              max: 200
          tag_length:
            inclusive:
              min: 3
              max: 150
          tags_per_msg:
            inclusive:
              min: 2
              max: 50
          multivalue_count:
            inclusive:
              min: 2
              max: 32
          multivalue_pack_probability: 0.08
          kind_weights:
            metric: 100
            event: 0
            service_check: 0
          metric_weights:
            count: 208
            gauge: 66
            timer: 0
            distribution: 72
            set: 9
            histogram: 1
      maximum_prebuild_cache_size_bytes: $tier_cache
      bytes_per_second: $tier_rate
EOF
}

# Prints ADP's telemetry once.
adp_scrape() {
  curl -fsS --max-time 5 "http://127.0.0.1:${ADP_TELEMETRY_PORT}/metrics"
}

# Prints, from ADP's telemetry on stdin: the metrics the DogStatsD source decoded, the metrics aggregation dropped, the
# contexts the source failed to resolve, and the datagram buffers in use and their capacity.
adp_values() {
  awk '
    $1 ~ /^adp__component_events_received_total\{/ && /component_id="dsd_in"/ && /message_type="metrics"/ {
      decoded += $NF }
    $1 ~ /^adp__component_events_dropped_total\{/ && /component_id="dsd_agg"/ { agg_dropped += $NF }
    $1 ~ /^adp__component_failed_context_resolve_total\{/ && /component_id="dsd_in"/ { unresolved += $NF }
    $1 ~ /^adp__object_pool_in_use\{/ && /pool_name="dsd_packet_bufs"/ { pool_in_use += $NF }
    $1 ~ /^adp__object_pool_capacity\{/ && /pool_name="dsd_packet_bufs"/ { pool_capacity += $NF }
    END { printf "%.0f %.0f %.0f %.0f %.0f\n", decoded, agg_dropped, unresolved, pool_in_use, pool_capacity }'
}

# Prints the bytes lading's generator has written so far.
lading_bytes() {
  curl -fsS --max-time 5 "http://127.0.0.1:${LADING_TELEMETRY_PORT}/metrics" |
    awk '$1 ~ /^bytes_written/ { sum += $NF } END { printf "%.0f\n", sum }'
}

# Prints ADP's cgroup counters, read from the host: CPU time (usec), CFS periods, throttled periods, throttled time
# (usec), and anonymous memory (bytes).
adp_cgroup() {
  awk '
    FILENAME ~ /cpu.stat$/ && $1 == "usage_usec" { usage = $2 }
    FILENAME ~ /cpu.stat$/ && $1 == "nr_periods" { periods = $2 }
    FILENAME ~ /cpu.stat$/ && $1 == "nr_throttled" { throttled = $2 }
    FILENAME ~ /cpu.stat$/ && $1 == "throttled_usec" { throttled_usec = $2 }
    FILENAME ~ /memory.stat$/ && $1 == "anon" { anon = $2 }
    END { printf "%d %d %d %d %d\n", usage, periods, throttled, throttled_usec, anon }' \
    "$adp_cgroup_dir/cpu.stat" "$adp_cgroup_dir/memory.stat"
}

# Samples lading and ADP's cgroup every second, and ADP's telemetry every $adp_sample seconds, into $1, until killed.
# With SCALING_KEEP_SCRAPES=1, each of ADP's scrapes is also kept whole in the directory $2.
sample() {
  local file=$1 scrapes=$2 tick=0 adp="- - - - -" scrape
  echo "t,lading_bytes,cpu_usec,periods,throttled,throttled_usec,anon_bytes,decoded,agg_dropped,unresolved,pool_in_use,pool_capacity" \
    >"$file"
  while true; do
    if ((tick % adp_sample == 0)); then
      scrape=$(adp_scrape 2>/dev/null || true)
      if [[ "$keep_scrapes" == 1 && -n "$scrape" ]]; then
        echo "$scrape" >"$scrapes/$(now).prom"
      fi
      adp=$(echo "$scrape" | adp_values || echo "- - - - -")
    else
      adp="- - - - -"
    fi
    echo "$(now),$(lading_bytes 2>/dev/null || echo -),$(adp_cgroup | tr ' ' ','),$(echo "$adp" | tr ' ' ',')" >>"$file"
    tick=$((tick + 1))
    sleep 1
  done
}

# Prints the counts `perf stat` wrote to $1 (CSV), in the order of $perf_events, with `-` for an event not counted.
perf_values() {
  local event values=()
  for event in ${perf_events//,/ }; do
    values+=("$(awk -F, -v e="$event" '$3 == e && $1 ~ /^[0-9]+$/ { print $1; found = 1 } END { if (!found) print "-" }' "$1")")
  done
  echo "${values[*]}"
}

# Runs one measured run of tier $1 at CPU limit $2, round $3, of image $5 labeled $4 with the environment file $6, in
# slot $7, appending a row to runs.csv.
measure() {
  local tier=$1 limit=$2 round=$3 label=$4 image=$5 env_file=$6 slot=$7
  local name="$tier-$limit-$round-$label"
  set_tier "$tier"
  use_slot "$slot"
  export ADP_IMAGE=$image
  export ADP_CPUS=$limit
  export ADP_VARIANT_ENV=$env_file
  export INTERNER_SIZE_BYTES=$tier_interner
  export LADING_CONFIG="$out/lading-$tier.yaml"
  log "$name ($image): slot $slot, ADP at $limit CPUs on cores $ADP_CPUSET, $mode at $tier_rate/s, $tier_contexts contexts"

  run "${compose[@]}" up --detach --wait adp blackhole
  run "${compose[@]}" up --detach lading
  if [[ "$dry_run" == 1 ]]; then
    echo "+ sleep $warmup; sample for $window seconds; ${compose[*]} down --volumes"
    return
  fi
  local adp_id
  adp_id=$(docker inspect --format '{{.Id}}' "$("${compose[@]}" ps --quiet adp)")
  adp_cgroup_dir="/sys/fs/cgroup/docker/$adp_id"
  [[ -r "$adp_cgroup_dir/cpu.stat" ]] || fail "cannot read ADP's cgroup at $adp_cgroup_dir"
  local git_hash
  # ADP logs JSON here (`"git_hash":"..."`); the text format writes `git_hash:"..."`.
  git_hash=$("${compose[@]}" logs --no-color adp 2>/dev/null | grep -oE 'git_hash"?:"[0-9a-f]+"' | head -1 |
    sed -E 's/.*:"([0-9a-f]+)"/\1/' || true)
  sleep "$warmup"

  if [[ "$keep_scrapes" == 1 ]]; then
    mkdir -p "$out/scrapes-$name"
  fi
  sample "$out/series-$name.csv" "$out/scrapes-$name" &
  local sampler=$! counter=""
  if [[ "$perf" == 1 ]]; then
    sudo -n perf stat -x, -o "$out/perf-$name.csv" -a -C "$ADP_CPUSET" --for-each-cgroup "docker/$adp_id" \
      -e "$perf_events" -- sleep "$window" &
    counter=$!
  fi
  local recorder=""
  if [[ "$record" == 1 ]]; then
    sudo -n perf record -q -o "$out/perf-$name.data" -a -C "$ADP_CPUSET" -e inst_retired -c 100000 \
      --cgroup "docker/$adp_id" --call-graph dwarf,8192 -- sleep "$record_seconds" 2>/dev/null &
    recorder=$!
  fi
  local t0 adp0 sent0 cgroup0 t1 adp1 sent1 cgroup1 peak counts="- - - - - -"
  t0=$(now)
  adp0=$(adp_scrape | adp_values)
  sent0=$(lading_bytes)
  cgroup0=$(adp_cgroup)
  sleep "$window"
  t1=$(now)
  adp1=$(adp_scrape | adp_values)
  sent1=$(lading_bytes)
  cgroup1=$(adp_cgroup)
  kill "$sampler" 2>/dev/null || true
  wait "$sampler" 2>/dev/null || true
  if [[ -n "$recorder" ]]; then
    wait "$recorder" || log "$name: perf record failed"
  fi
  if [[ -n "$counter" ]]; then
    wait "$counter" || log "$name: perf stat failed"
    counts=$(perf_values "$out/perf-$name.csv")
  fi
  peak=$(cat "$adp_cgroup_dir/memory.peak" 2>/dev/null || cat "$adp_cgroup_dir/memory.current")
  "${compose[@]}" logs --no-color adp >"$out/adp-$name.log" 2>&1 || true
  "${compose[@]}" down --volumes --timeout 30 >/dev/null

  read -r decoded0 dropped0 unresolved0 _ _ <<<"$adp0"
  read -r decoded1 dropped1 unresolved1 _ _ <<<"$adp1"
  read -r usage0 periods0 throttled0 throttled_usec0 _ <<<"$cgroup0"
  read -r usage1 periods1 throttled1 throttled_usec1 _ <<<"$cgroup1"
  echo "$tier,$limit,$round,$label,$image,$git_hash,$mode,$(size_bytes "$tier_rate"),$tier_contexts,$tier_memory_bound_mib,$(python3 -I -c "print(f'{$t1 - $t0:.3f}')"),$((decoded1 - decoded0)),$((sent1 - sent0)),$((usage1 - usage0)),$((periods1 - periods0)),$((throttled1 - throttled0)),$((throttled_usec1 - throttled_usec0)),$((dropped1 - dropped0)),$((unresolved1 - unresolved0)),$peak,series-$name.csv,$slot,$reference,${counts// /,}" \
    >>"$out/runs.csv"
}

# Runs the jobs of slot $1, one after another: each job is `tier limit round label`, from $jobs_dir/slot-$1.
run_slot() {
  local slot=$1 tier limit round label
  while read -r tier limit round label; do
    measure "$tier" "$limit" "$round" "$label" "${variant_image[$label]}" "$out/env-$label.env" "$slot"
  done <"$jobs_dir/slot-$slot"
}

main() {
  case "$mode" in
    paced | saturate) ;;
    *) fail "SCALING_MODE is paced or saturate, not '$mode'" ;;
  esac
  export ADP_CONFIG="$here/adp/defaults.yaml"
  export ADP_PROFILE_ENV="$here/adp/defaults.env"
  export ADP_TRANSPORT_ENV="$here/adp/unix_datagram.env"

  local host_cpus
  host_cpus=$(getconf _NPROCESSORS_ONLN)
  if ((host_cpus < parallel * slot_width)); then
    fail "this host has $host_cpus CPUs; $parallel slot(s) of $slot_width cores need $((parallel * slot_width))"
  fi
  if [[ ("$perf" == 1 || "$record" == 1) && "$dry_run" != 1 ]]; then
    sudo -n perf --version >/dev/null 2>&1 || fail "SCALING_PERF=1 and SCALING_RECORD=1 need perf and passwordless sudo"
  fi

  # The cells: `tier:limit` pairs, from SCALING_CELLS or every tier at every limit.
  local cell_list=() tier limit
  if [[ -n "$cells" ]]; then
    read -r -a cell_list <<<"$cells"
  else
    for tier in $tiers; do
      for limit in $limits; do
        cell_list+=("$tier:$limit")
      done
    done
  fi

  # The variants: label, image, and environment, from SCALING_VARIANTS or main and plan (main against main for A/A).
  declare -gA variant_image=()
  local variant_list=() spec label rest image
  if [[ -n "$variants" ]]; then
    read -r -a variant_list <<<"$variants"
  elif [[ "$aa" == 1 ]]; then
    variant_list=("main-a=$main_image" "main-b=$main_image")
  else
    variant_list=("main=$main_image" "plan=$plan_image")
  fi
  ((${#variant_list[@]} >= 2)) || fail "SCALING_VARIANTS names at least two variants"
  mkdir -p "$out"
  local labels=()
  for spec in "${variant_list[@]}"; do
    label=${spec%%=*}
    rest=${spec#*=}
    image=${rest%%;*}
    [[ "$label" =~ ^[a-z0-9][a-z0-9_.-]*$ ]] || fail "variant label '$label' is not lowercase letters, digits, _, ., -"
    [[ -z "${variant_image[$label]:-}" ]] || fail "variant label '$label' is named twice"
    variant_image[$label]=$image
    labels+=("$label")
    : >"$out/env-$label.env"
    if [[ "$rest" == *";"* ]]; then
      tr ';' '\n' <<<"${rest#*;}" | sed '/^$/d' >"$out/env-$label.env"
    fi
    if [[ "$dry_run" != 1 ]]; then
      docker image inspect "$image" >/dev/null 2>&1 || fail "image $image is missing; README.md says how to build both"
    fi
  done
  reference=${labels[0]}

  # One lading configuration per tier, rendered before any slot starts.
  local cell
  for cell in "${cell_list[@]}"; do
    tier=${cell%%:*}
    set_tier "$tier"
    [[ -f "$out/lading-$tier.yaml" ]] || render_lading_config "$out/lading-$tier.yaml"
  done

  # The jobs: per cell and round, every variant, rotating which goes first; a cell's round stays in one slot, so its
  # variants run back to back on the same cores.
  jobs_dir=$(mktemp -d)
  local group=0 round index count=${#labels[@]}
  for cell in "${cell_list[@]}"; do
    tier=${cell%%:*}
    limit=${cell#*:}
    for ((round = 1; round <= rounds; round++)); do
      for ((index = 0; index < count; index++)); do
        echo "$tier $limit $round ${labels[$(((index + round - 1) % count))]}" >>"$jobs_dir/slot-$((group % parallel))"
      done
      group=$((group + 1))
    done
  done

  if [[ "$dry_run" != 1 ]]; then
    # Tear down whatever runs were in progress if the script stops early.
    trap 'for slot in $(seq 0 $((parallel - 1))); do use_slot "$slot"; "${compose[@]}" down --volumes --timeout 10 \
      >/dev/null 2>&1 || true; done; rm -rf "$jobs_dir"' EXIT
  fi
  echo "tier,limit,round,label,image,git_hash,mode,target_bytes_per_second,contexts,memory_bound_mib,seconds,decoded,lading_bytes,cpu_usec,periods,throttled,throttled_usec,agg_dropped,unresolved,memory_peak_bytes,series,slot,reference,instructions,cycles,l1d_refills,l2d_refills,stall_backend,dtlb_walks" \
    >"$out/runs.csv"
  log "writing to $out: ${#cell_list[@]} cell(s), variants ${labels[*]}, $mode, $rounds round(s), $parallel slot(s)"
  local slot pids=()
  for ((slot = 0; slot < parallel; slot++)); do
    [[ -f "$jobs_dir/slot-$slot" ]] || continue
    run_slot "$slot" &
    pids+=($!)
  done
  local status=0 pid
  for pid in "${pids[@]}"; do
    wait "$pid" || status=1
  done
  ((status == 0)) || fail "a slot failed; see the log above"
  if [[ "$dry_run" != 1 ]]; then
    python3 -I "$here/summarize_limits.py" "$out/runs.csv" | tee "$out/summary.md"
  fi
}

main "$@"
