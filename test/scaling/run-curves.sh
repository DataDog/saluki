#!/usr/bin/env bash
#
# Runs the scaling curves of plan section 8.4 in containers: for each N in SCALING_WORKERS, ADP under N CPUs with
# SALUKI_WORKER_THREADS=N, lading on the next two CPUs, and the blackholes on the two after, for SCALING_ROUNDS rounds of
# each image, alternating which goes first. Each run waits SCALING_WARMUP_SECONDS, then reads, at both ends of a
# window of SCALING_WINDOW_SECONDS, the metrics ADP's DogStatsD source decoded, ADP's container CPU time (cgroup
# `cpu.stat`), and what lading sent; then the container's peak memory. Results go to SCALING_OUT/runs.csv, and
# summarize.py prints the medians, ratios, and scaling. See README.md for the variables, and for which configuration of
# the gate each combination measures.
#
# SCALING_DRY_RUN=1 renders every run's lading configuration and prints the docker commands without running them.

set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

workers=${SCALING_WORKERS:-"1 2 4 8 16"}
rounds=${SCALING_ROUNDS:-5}
legacy_image=${SCALING_LEGACY_IMAGE:-saluki-images/agent-data-plane:legacy-b940bf1f8b}
plan_image=${SCALING_PLAN_IMAGE:-saluki-images/agent-data-plane:plan}
aa=${SCALING_AA:-0}
warmup=${SCALING_WARMUP_SECONDS:-20}
window=${SCALING_WINDOW_SECONDS:-60}
rate=${SCALING_RATE:-"4 GiB"}
generators=${SCALING_GENERATORS:-2}
connections=${SCALING_CONNECTIONS:-1}
contexts=${SCALING_CONTEXTS:-3000}
profile=${SCALING_PROFILE:-defaults}
adp_config=${SCALING_ADP_CONFIG:-}
transport=${SCALING_TRANSPORT:-unix_datagram}
metric_weights=${SCALING_METRIC_WEIGHTS:-"208:66:72:9:1"}
timestamp_probability=${SCALING_TIMESTAMP_PROBABILITY:-0}
cache=${SCALING_CACHE:-"500 Mb"}
decoders_override=${SCALING_DECODERS:-}
plan_cpu_time_sampling=${SCALING_PLAN_CPU_TIME_SAMPLING:-1}
dry_run=${SCALING_DRY_RUN:-0}
out=${SCALING_OUT:-"$here/results/$(date +%Y%m%dT%H%M%S)"}

project="saluki-scaling-$$"
compose=(docker compose --file "$here/compose.yaml" --project-name "$project")

log() {
  echo "[scaling] $*" >&2
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

# Prints the CPU set from $1 to $2, as `docker run --cpuset-cpus` takes it.
cpu_range() {
  if (($1 == $2)); then
    echo "$1"
  else
    echo "$1-$2"
  fi
}

# Prints the size $1 (such as `500 MiB`, or `500 Mb` in bits, as lading reads a lowercase b) divided by $2, at least
# $3 bytes, as a lading byte size.
divide_bytes() {
  python3 -I - "$1" "$2" "$3" <<'EOF'
import re, sys
size, divisor, floor = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
match = re.fullmatch(r"\s*([0-9.]+)\s*([KMG]?)(i?)([Bb])\s*", size)
if not match:
    sys.exit(f"a size looks like '500 MiB' or '500 Mb', not {size!r}")
number, prefix, binary, unit = match.groups()
power = {"": 0, "K": 1, "M": 2, "G": 3}[prefix]
factor = (1024 if binary else 1000) ** power / (8 if unit == "b" else 1)
print(f"{max(floor, int(float(number) * factor / divisor)) // 1024} KiB")
EOF
}

# Prints the lading `metric_weights` of $metric_weights (count:gauge:distribution:set:histogram), one field a line.
render_metric_weights() {
  python3 -I - "$metric_weights" <<'EOF'
import sys
weights = sys.argv[1].split(":")
if len(weights) != 5 or not all(w.isdigit() and int(w) <= 255 for w in weights) or not any(int(w) for w in weights):
    sys.exit(f"SCALING_METRIC_WEIGHTS is count:gauge:distribution:set:histogram, each 0 to 255, not {sys.argv[1]!r}")
count, gauge, distribution, set_, histogram = weights
for name, weight in [("count", count), ("gauge", gauge), ("timer", "0"), ("distribution", distribution),
                     ("set", set_), ("histogram", histogram)]:
    print(f"            {name}: {weight}")
EOF
}

# Prints the lading `metric_names` of the profile: the names its ADP configuration acts on (see adp/<profile>.yaml).
render_metric_names() {
  local names=()
  case "$profile" in
    defaults) return ;;
    mapper) names=('raw.requests.route{{0-15}}' 'raw.blocked' 'app.queue{{0-31}}') ;;
    gateways)
      names=('checkout.latency' 'system.disk{{0-7}}' 'app.queue{{0-15}}' 'raw.requests.route{{0-15}}' 'raw.blocked'
        'other.metric{{0-15}}')
      ;;
  esac
  echo "          metric_names:"
  local name
  for name in "${names[@]}"; do
    echo "            - \"$name\""
  done
}

# Writes the lading configuration of a run to $1: $generators entries over the shared socket, each with $connections
# connections, each connection sending its share of $rate, over $contexts contexts.
render_lading_config() {
  local file=$1
  local share entry_cache framing=""
  share=$(divide_bytes "$rate" $((generators * connections)) 1024)
  # lading keeps a connection's rate in 32 bits: below 4 GiB per second.
  if ((${share% KiB} > 4194303)); then
    log "SCALING_RATE gives each connection ${share}/s, more than lading takes: each connection sends 4 GiB/s at most"
    share="4194303 KiB"
  fi
  entry_cache=$cache
  if [[ "$transport" == unix_stream ]]; then
    # Each stream connection builds a block cache of its own; datagram connections share their entry's.
    entry_cache=$(divide_bytes "$cache" "$connections" $((1 << 20)))
    framing=$'\n          length_prefix_framed: true'
  fi
  local weights names timestamp=""
  weights=$(render_metric_weights)
  names=$(render_metric_names)
  if [[ "$timestamp_probability" != 0 ]]; then
    timestamp=$'\n          timestamp:\n            range:\n              constant: 1700000000'
    timestamp+=$'\n            probability: '"$timestamp_probability"
  fi
  {
    echo "# Rendered by run-curves.sh: $generators $transport generator(s) of $connections connection(s) at $share/s each,"
    echo "# $contexts contexts, profile $profile."
    echo "generator:"
    for ((index = 0; index < generators; index++)); do
      cat <<EOF
  - $transport:
      seed: [$((2 + index)), 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97,
        101, 103, 107, 109, 113, 127, 131]
      path: /var/run/dsd/dsd.sock
      block_cache_method: Fixed
      parallel_connections: $connections
      variant:
        dogstatsd:
          contexts:
            inclusive:
              min: $contexts
              max: $((contexts + 1))
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
$weights${names:+
$names}$timestamp$framing
      maximum_prebuild_cache_size_bytes: $entry_cache
      maximum_block_size: 8 KiB
      bytes_per_second: $share
EOF
    done
  } >"$file"
}

# Prints the metrics ADP's DogStatsD source has decoded so far.
adp_decoded() {
  curl -fsS "http://127.0.0.1:${ADP_TELEMETRY_PORT}/metrics" |
    awk '$1 ~ /^adp__component_events_received_total\{/ && /component_id="dsd_in"/ && /message_type="metrics"/ {
      sum += $NF } END { printf "%.0f\n", sum }'
}

# Prints the CPU time, in microseconds, ADP's container has used so far.
adp_cpu_usec() {
  "${compose[@]}" exec -T adp cat /sys/fs/cgroup/cpu.stat | awk '$1 == "usage_usec" { print $2 }'
}

# Prints the peak memory of ADP's container, in bytes (cgroup v2; `memory.current` on kernels without `memory.peak`).
adp_memory_peak() {
  "${compose[@]}" exec -T adp sh -c 'cat /sys/fs/cgroup/memory.peak 2>/dev/null || cat /sys/fs/cgroup/memory.current'
}

# Prints the bytes lading's generators have written so far.
lading_bytes() {
  curl -fsS "http://127.0.0.1:${LADING_TELEMETRY_PORT}/metrics" |
    awk '$1 ~ /^bytes_written/ { sum += $NF } END { printf "%.0f\n", sum }'
}

# Runs one measured run of image $3, labeled $2, at $1 CPUs, for round $4, appending a row to runs.csv.
measure() {
  local cpus=$1 label=$2 image=$3 round=$4
  local decoders=${decoders_override:-$((cpus > 4 ? cpus - 2 : 2))}
  export ADP_IMAGE=$image
  export ADP_CPUS=$cpus
  export ADP_CPUSET=$(cpu_range 0 $((cpus - 1)))
  export LADING_CPUSET=$(cpu_range "$cpus" $((cpus + 1)))
  export BLACKHOLE_CPUSET=$(cpu_range $((cpus + 2)) $((cpus + 3)))
  export WORKERS=$cpus
  export DECODERS=$decoders
  export CONTEXT_LIMIT=$((contexts + contexts / 10 + 100))
  export LADING_CONFIG="$out/lading-$cpus.yaml"
  export ADP_TELEMETRY_PORT=${ADP_TELEMETRY_PORT:-15100}
  export LADING_TELEMETRY_PORT=${LADING_TELEMETRY_PORT:-19229}
  export BLACKHOLE_TELEMETRY_PORT=${BLACKHOLE_TELEMETRY_PORT:-19230}
  if [[ "$label" == plan* ]]; then
    export CPU_TIME_SAMPLING=$plan_cpu_time_sampling
  else
    export CPU_TIME_SAMPLING=
  fi
  render_lading_config "$LADING_CONFIG"
  log "N=$cpus round $round $label ($image): ADP on CPUs $ADP_CPUSET, $decoders decoders, $profile over $transport"

  run "${compose[@]}" up --detach --wait adp blackhole
  run "${compose[@]}" up --detach lading
  if [[ "$dry_run" == 1 ]]; then
    echo "+ sleep $warmup; read counters; sleep $window; read counters; ${compose[*]} down --volumes"
    return
  fi
  sleep "$warmup"
  local t0 decoded0 cpu0 sent0 t1 decoded1 cpu1 sent1 peak
  t0=$(now)
  decoded0=$(adp_decoded)
  cpu0=$(adp_cpu_usec)
  sent0=$(lading_bytes)
  sleep "$window"
  t1=$(now)
  decoded1=$(adp_decoded)
  cpu1=$(adp_cpu_usec)
  sent1=$(lading_bytes)
  peak=$(adp_memory_peak)
  "${compose[@]}" logs --no-color adp >"$out/adp-$cpus-$round-$label.log" 2>&1 || true
  "${compose[@]}" down --volumes --timeout 30 >/dev/null
  echo "$cpus,$round,$label,$image,$(python3 -I -c "print(f'{$t1 - $t0:.3f}')"),$((decoded1 - decoded0)),$((cpu1 - cpu0)),$((sent1 - sent0)),$peak,$profile,$transport,$((generators * connections))" \
    >>"$out/runs.csv"
}

main() {
  case "$profile" in
    defaults | mapper | gateways) ;;
    *) fail "SCALING_PROFILE is defaults, mapper, or gateways, not '$profile'" ;;
  esac
  case "$transport" in
    unix_datagram | unix_stream) ;;
    *) fail "SCALING_TRANSPORT is unix_datagram or unix_stream, not '$transport'" ;;
  esac
  export ADP_CONFIG=${adp_config:-"$here/adp/$profile.yaml"}
  [[ -f "$ADP_CONFIG" ]] || fail "SCALING_ADP_CONFIG names $ADP_CONFIG, which does not exist"
  if [[ "$profile" == gateways ]]; then
    export ADP_PROFILE_ENV="$here/adp/gateways.env"
  else
    export ADP_PROFILE_ENV="$here/adp/defaults.env"
  fi
  export ADP_TRANSPORT_ENV="$here/adp/$transport.env"

  mkdir -p "$out"
  local max_workers=0 cpus
  for cpus in $workers; do
    if ((cpus > max_workers)); then
      max_workers=$cpus
    fi
  done
  local host_cpus
  host_cpus=$(getconf _NPROCESSORS_ONLN)
  if ((host_cpus < max_workers + 4)) && [[ "${SCALING_ALLOW_SMALL_HOST:-0}" != 1 ]]; then
    log "this host has $host_cpus CPUs; N=$max_workers needs $((max_workers + 4)) (N for ADP, 2 for lading, 2 for the"
    log "blackholes). Lower SCALING_WORKERS, or set SCALING_ALLOW_SMALL_HOST=1 to overlap CPU sets (not for gating)."
    exit 1
  fi

  local first_label=legacy first_image=$legacy_image second_label=plan second_image=$plan_image
  if [[ "$aa" == 1 ]]; then
    first_label=legacy-a
    second_label=legacy-b
    second_image=$legacy_image
  fi
  if [[ "$dry_run" != 1 ]]; then
    for image in "$first_image" "$second_image"; do
      docker image inspect "$image" >/dev/null 2>&1 || fail "image $image is missing; README.md says how to build both"
    done
  fi

  echo "cpus,round,label,image,seconds,decoded,cpu_usec,lading_bytes,memory_peak_bytes,profile,transport,connections" \
    >"$out/runs.csv"
  log "writing to $out: profile $profile ($ADP_CONFIG) over $transport, $generators x $connections connection(s)"
  local round
  for cpus in $workers; do
    for ((round = 1; round <= rounds; round++)); do
      if ((round % 2 == 1)); then
        measure "$cpus" "$first_label" "$first_image" "$round"
        measure "$cpus" "$second_label" "$second_image" "$round"
      else
        measure "$cpus" "$second_label" "$second_image" "$round"
        measure "$cpus" "$first_label" "$first_image" "$round"
      fi
    done
  done
  if [[ "$dry_run" != 1 ]]; then
    python3 -I "$here/summarize.py" "$out/runs.csv" | tee "$out/summary.md"
  fi
}

main "$@"
