#!/usr/bin/env bash
#
# Runs N1's measurements in the production regime (docs/design/programmable-metrics-next.md): ADP under a fractional
# CPU limit on two cores with two workers (what ADP picks under any quota up to two CPUs), DogStatsD over UDP at a
# steady paced rate, then a burst of several times that rate timed across an aggregation flush. For each limit in
# REGIME_CPUS and each round, every label in REGIME_LABELS runs once, in an order that rotates by round; a label names
# its image by its prefix (`native*` is the native image, `plan*` the plan image, `candidate*` a change judged against
# the plan image), so that `native-a native-b plan-a plan-b` gives an A/A pair of each image beside the comparison. A
# prefix that REGIME_VARIANTS names runs the candidate image with the configuration it gives.
#
# Each run: ADP, the blackholes, and the sampler (`regime_sampler.py`, which reads ADP's cgroup and UDP counters from
# outside its container every 100 ms) start; the steady generator and the burst generator start, and the burst
# generator is paused (`docker compose pause`) once it sends. After REGIME_WARMUP_SECONDS, ADP's telemetry and lading's
# counters are read at both ends of a steady window of REGIME_STEADY_SECONDS. The script then waits until
# REGIME_BURST_LEAD_SECONDS before a flush of the aggregation (whose timer starts when ADP's topology does: every
# REGIME_FLUSH_SECONDS from the log line that says so), unpauses the burst generator for REGIME_BURST_SECONDS, pauses
# it again, and reads everything again after REGIME_RECOVERY_SECONDS more. The sampler's phases come from a file the
# script writes into the sampler's volume.
#
# Results go to REGIME_OUT: runs.csv (one row per run, with the readings at the phase boundaries), each run's
# timeline-<run>.csv, ADP log, and the rendered lading configurations. summarize-regime.py prints the tables.

set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# The helpers (render_lading_config, divide_bytes, now, cpu_range) and their variables.
# shellcheck source=run-curves.sh
source "$here/run-curves.sh"

cpu_limits=${REGIME_CPUS:-"0.05 0.3 0.6 1.2"}
labels=${REGIME_LABELS:-"native-a native-b plan-a plan-b"}
rounds=${REGIME_ROUNDS:-3}
native_image=${REGIME_NATIVE_IMAGE:-saluki-images/agent-data-plane:legacy-b940bf1f8b}
plan_image=${REGIME_PLAN_IMAGE:-saluki-images/agent-data-plane:plan}
# A change judged against the plan image (`candidate*` labels), with the plan image's configuration unless
# REGIME_CANDIDATE_CONFIG names another.
candidate_image=${REGIME_CANDIDATE_IMAGE:-}
plan_config=${REGIME_PLAN_CONFIG:-"$here/adp/lanes-off.yaml"}
native_config=${REGIME_NATIVE_CONFIG:-"$here/adp/defaults.yaml"}
candidate_config=${REGIME_CANDIDATE_CONFIG:-$plan_config}
# Further label prefixes for the candidate image, each with its own configuration: `name=path` pairs, such as
# `fused=adp/lanes-off.yaml isolated=adp/isolated.yaml` for labels `fused-a fused-b isolated-a isolated-b`.
variants=${REGIME_VARIANTS:-}
steady_per_cpu=${REGIME_STEADY_RATE_PER_CPU:-"60 MiB"}
# Steady rates per limit, `cpus:KiB/s` pairs, such as `0.3:23295 1.2:209900`; a limit not listed gets
# REGIME_STEADY_RATE_PER_CPU times its CPUs. N1 sets each to 30% of the native image's saturated rate at that limit.
steady_rates=${REGIME_STEADY_RATES:-}
burst_factor=${REGIME_BURST_FACTOR:-4}
warmup=${REGIME_WARMUP_SECONDS:-20}
steady_window=${REGIME_STEADY_SECONDS:-30}
flush_interval=${REGIME_FLUSH_SECONDS:-15}
burst_lead=${REGIME_BURST_LEAD_SECONDS:-3}
burst_seconds=${REGIME_BURST_SECONDS:-8}
recovery=${REGIME_RECOVERY_SECONDS:-12}
adp_cpuset=${REGIME_ADP_CPUSET:-0-1}
lading_cpuset=${REGIME_LADING_CPUSET:-2-3}
sampler_cpuset=${REGIME_SAMPLER_CPUSET:-4-5}
regime_workers=${REGIME_WORKERS:-2}
regime_decoders=${REGIME_DECODERS:-2}
# The helpers' variables: one UDP generator entry per container, over REGIME_CONTEXTS contexts.
transport=${REGIME_TRANSPORT:-unix_datagram}
generators=1
connections=1
contexts=${REGIME_CONTEXTS:-3000}
context_limit=${REGIME_CONTEXT_LIMIT:-$((contexts * 10 + 1000))}
cache=${REGIME_CACHE:-"500 Mb"}
out=${REGIME_OUT:-"$here/results/regime-$(date +%Y%m%dT%H%M%S)"}
project="saluki-regime-$$"
compose=(docker compose --file "$here/compose.yaml" --project-name "$project" --profile regime)

# Prints $1 bytes per second (a lading size) times $2, as a lading size in KiB.
scale_bytes() {
  python3 -I - "$1" "$2" <<'EOF'
import re, sys
size, factor = sys.argv[1], float(sys.argv[2])
match = re.fullmatch(r"\s*([0-9.]+)\s*([KMG]?)(i?)([Bb])\s*", size)
if not match:
    sys.exit(f"a size looks like '60 MiB', not {size!r}")
number, prefix, binary, unit = match.groups()
power = {"": 0, "K": 1, "M": 2, "G": 3}[prefix]
total = float(number) * (1024 if binary else 1000) ** power / (8 if unit == "b" else 1) * factor
print(f"{max(1, int(total // 1024))} KiB")
EOF
}

# Prints the sum of ADP's telemetry series named $1 whose labels match the awk regular expression $2 (all if empty).
adp_series() {
  curl -fsS --max-time 20 "http://127.0.0.1:${ADP_TELEMETRY_PORT}/metrics" |
    awk -v name="$1" -v labels="${2:-}" '
      { split($1, parts, "{"); }
      parts[1] == name && (labels == "" || $1 ~ labels) { sum += $NF; found = 1 }
      END { if (found) printf "%.0f\n", sum; else print "" }'
}

# Saves ADP's whole telemetry to $1 (read at the phase boundaries only: a scrape spends ADP's CPU quota).
adp_scrape() {
  curl -fsS --max-time 20 "http://127.0.0.1:${ADP_TELEMETRY_PORT}/metrics" >"$1" 2>/dev/null || : >"$1"
}

# Prints the sum of series $2 (labels matching $3) in the scrape $1.
scraped() {
  awk -v name="$2" -v labels="${3:-}" '
    { split($1, parts, "{"); }
    parts[1] == name && (labels == "" || $1 ~ labels) { sum += $NF; found = 1 }
    END { if (found) printf "%.0f\n", sum; else print "" }' "$1"
}

# Prints the bytes the lading container on telemetry port $1 has written so far.
# A paused container does not answer: then it prints nothing.
lading_written() {
  { curl -fsS --max-time 2 "http://127.0.0.1:$1/metrics" 2>/dev/null || true; } |
    awk '$1 ~ /^bytes_written/ { sum += $NF; found = 1 } END { if (found) printf "%.0f\n", sum; else print "" }'
}

# Tells the sampler the run's phase (a file in its volume; see compose.yaml).
phase() {
  "${compose[@]}" exec -T sampler sh -c "echo $1 >/out/phase" >/dev/null 2>&1 || true
}

# Prints the wall-clock time at which ADP's aggregation flush timer started: the time of ADP's log line `Agent Data Plane
# running.` (both images log it once the topology is spawned), from `docker compose logs --timestamps`.
flush_origin() {
  "${compose[@]}" logs --no-color --timestamps adp 2>/dev/null |
    python3 -I -c '
import re, sys
from datetime import datetime, timezone
for line in sys.stdin:
    if "Agent Data Plane running." in line:
        stamp = re.search(r"(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(\.\d+)?Z", line)
        if stamp:
            base = datetime.strptime(stamp.group(1), "%Y-%m-%dT%H:%M:%S").replace(tzinfo=timezone.utc)
            print(f"{base.timestamp() + float(stamp.group(2) or 0):.3f}")
            break'
}

# Sleeps until the wall-clock time $1.
sleep_until() {
  local delay
  delay=$(python3 -I -c "import time; print(max(0.0, $1 - time.time()))")
  sleep "$delay"
}

# Records the readings named $1 into the associative array `reading`: ADP's telemetry and both generators' bytes.
read_point() {
  local tag=$1 scrape="$run_dir/scrape-$1.txt"
  reading[${tag}_t]=$(now)
  adp_scrape "$scrape"
  reading[${tag}_decoded]=$(scraped "$scrape" adp__component_events_received_total 'component_id="dsd_in".*message_type="metrics"|message_type="metrics".*component_id="dsd_in"')
  reading[${tag}_pool_capacity]=$(scraped "$scrape" adp__object_pool_capacity 'pool_name="dsd_packet_bufs"')
  reading[${tag}_pool_in_use]=$(scraped "$scrape" adp__object_pool_in_use 'pool_name="dsd_packet_bufs"')
  reading[${tag}_pool_created]=$(scraped "$scrape" adp__object_pool_created 'pool_name="dsd_packet_bufs"')
  reading[${tag}_flushes]=$(scraped "$scrape" adp__aggregate_flushes_total)
  reading[${tag}_contexts]=$(scraped "$scrape" adp__aggregate_active_contexts)
  reading[${tag}_steady_sent]=$(lading_written "$LADING_TELEMETRY_PORT")
}

# Runs one measured run of label $2 at $1 CPUs in round $3, appending a row to runs.csv.
measure() {
  local cpus=$1 label=$2 round=$3 image config
  case "$label" in
    native*) image=$native_image config=$native_config ;;
    plan*) image=$plan_image config=$plan_config ;;
    candidate*) image=$candidate_image config=$candidate_config ;;
    *)
      local pair
      for pair in $variants; do
        [[ "${label%%-*}" != "${pair%%=*}" ]] || image=$candidate_image config=${pair#*=}
      done
      [[ -n "${config:-}" ]] || fail "a label starts with native, plan, candidate, or a variant, not '$label'"
      [[ "$config" == /* ]] || config="$here/$config"
      ;;
  esac
  # A series resumes where it stopped (a rerun with the same REGIME_OUT): a run with a row in runs.csv is done.
  if [[ -f "$out/runs.csv" ]] && grep -q "^$cpus,$round,$label," "$out/runs.csv"; then
    log "$cpus CPUs round $round $label: done already"
    return
  fi
  run_dir="$out/runs/$cpus-$round-$label"
  rm -rf "$run_dir"
  mkdir -p "$run_dir"
  export ADP_IMAGE=$image ADP_CONFIG=$config ADP_CPUS=$cpus ADP_CPUSET=$adp_cpuset LADING_CPUSET=$lading_cpuset
  export BLACKHOLE_CPUSET=$sampler_cpuset WORKERS=$regime_workers DECODERS=$regime_decoders CONTEXT_LIMIT=$context_limit
  export CPU_TIME_SAMPLING= DOGSTATSD_PORT=0
  [[ "$transport" != udp ]] || DOGSTATSD_PORT=8125
  export LADING_CONFIG="$out/lading-steady-$cpus.yaml" BURST_CONFIG="$out/lading-burst-$cpus.yaml"
  local steady burst pair
  steady=$(scale_bytes "$steady_per_cpu" "$cpus")
  for pair in $steady_rates; do
    [[ "${pair%%:*}" != "$cpus" ]] || steady="${pair#*:} KiB"
  done
  burst="$(python3 -I -c "print(max(1, int(${steady% KiB} * ($burst_factor - 1))))") KiB"
  rate=$steady render_lading_config "$LADING_CONFIG"
  rate=$burst render_lading_config "$BURST_CONFIG"
  log "$cpus CPUs round $round $label ($image): steady $steady/s, burst +$burst/s, $contexts contexts"

  "${compose[@]}" up --detach --wait adp blackhole sampler >/dev/null 2>&1
  phase boot
  "${compose[@]}" up --detach lading burst >/dev/null 2>&1
  # Pause the burst generator as soon as it sends (it builds its block cache first).
  local waited=0
  until (($(lading_written "$BURST_TELEMETRY_PORT") + 0 > 0)); do
    sleep 0.2
    waited=$((waited + 1))
    ((waited < 900)) || fail "the burst generator never sent"
  done
  "${compose[@]}" pause burst >/dev/null 2>&1
  until (($(lading_written "$LADING_TELEMETRY_PORT") + 0 > 0)); do
    sleep 0.2
  done
  phase warmup
  sleep "$warmup"

  declare -A reading=()
  phase steady
  read_point s0
  sleep "$steady_window"
  read_point s1

  # The next flush at least REGIME_BURST_LEAD_SECONDS + 1 s away.
  local origin flush
  origin=$(flush_origin)
  [[ -n "$origin" ]] || fail "no topology start line in ADP's log"
  flush=$(python3 -I -c "
import math, time
origin, interval, lead = $origin, $flush_interval, $burst_lead
k = math.ceil((time.time() + lead + 1 - origin) / interval)
print(f'{origin + k * interval:.3f}')")
  phase align
  sleep_until "$(python3 -I -c "print($flush - $burst_lead)")"
  phase burst
  read_point b0
  "${compose[@]}" unpause burst >/dev/null 2>&1
  reading[burst_t0]=$(now)
  reading[burst_sent0]=$(lading_written "$BURST_TELEMETRY_PORT")
  sleep "$burst_seconds"
  reading[burst_t1]=$(now)
  reading[burst_sent1]=$(lading_written "$BURST_TELEMETRY_PORT")
  "${compose[@]}" pause burst >/dev/null 2>&1
  phase recovery
  read_point b1
  sleep "$recovery"
  read_point b2
  phase done
  sleep 0.3

  "${compose[@]}" logs --no-color --timestamps adp >"$run_dir/adp.log" 2>&1 || true
  "${compose[@]}" cp sampler:/out/timeline.csv "$run_dir/timeline.csv" >/dev/null 2>&1
  "${compose[@]}" unpause burst >/dev/null 2>&1 || true
  "${compose[@]}" down --volumes --timeout 10 >/dev/null 2>&1

  local fields=(cpus round label image contexts origin flush)
  local values=("$cpus" "$round" "$label" "$image" "$contexts" "$origin" "$flush")
  local tag key
  for tag in s0 s1 b0 b1 b2; do
    for key in t decoded pool_capacity pool_in_use pool_created flushes contexts steady_sent; do
      fields+=("${tag}_$key")
      values+=("${reading[${tag}_$key]:-}")
    done
  done
  fields+=(burst_t0 burst_sent0 burst_t1 burst_sent1 steady_rate burst_rate)
  values+=("${reading[burst_t0]:-}" "${reading[burst_sent0]:-}" "${reading[burst_t1]:-}" "${reading[burst_sent1]:-}"
    $((${steady% KiB} * 1024)) $((${burst% KiB} * 1024)))
  if [[ ! -f "$out/runs.csv" ]]; then
    (IFS=,; echo "${fields[*]}") >"$out/runs.csv"
  fi
  (IFS=,; echo "${values[*]}") >>"$out/runs.csv"
}

regime_main() {
  trap '"${compose[@]}" unpause burst >/dev/null 2>&1 || true; "${compose[@]}" down --volumes --timeout 5 >/dev/null 2>&1 || true' EXIT
  local images=("$native_image" "$plan_image")
  if [[ "$labels" == *candidate* || -n "$variants" ]]; then
    [[ -n "$candidate_image" ]] || fail "candidate labels need REGIME_CANDIDATE_IMAGE"
    images+=("$candidate_image")
  fi
  for image in "${images[@]}"; do
    docker image inspect "$image" >/dev/null 2>&1 || fail "image $image is missing"
  done
  export ADP_PROFILE_ENV="$here/adp/defaults.env" ADP_TRANSPORT_ENV="$here/adp/$transport.env"
  export ADP_TELEMETRY_PORT=${ADP_TELEMETRY_PORT:-15100} LADING_TELEMETRY_PORT=${LADING_TELEMETRY_PORT:-19229}
  export BLACKHOLE_TELEMETRY_PORT=${BLACKHOLE_TELEMETRY_PORT:-19230} BURST_TELEMETRY_PORT=${BURST_TELEMETRY_PORT:-19231}
  mkdir -p "$out"
  [[ ! -f "$out/series.txt" ]] || cp "$out/series.txt" "$out/series.previous.txt"
  {
    echo "cpus: $cpu_limits"
    echo "labels: $labels"
    echo "rounds: $rounds"
    echo "native_image: $native_image ($(docker image inspect --format '{{.Id}}' "$native_image"))"
    echo "plan_image: $plan_image ($(docker image inspect --format '{{.Id}}' "$plan_image"))"
    echo "plan_config: $plan_config"
    [[ -z "$candidate_image" ]] || echo "candidate_image: $candidate_image ($(docker image inspect --format '{{.Id}}' "$candidate_image")), config $candidate_config"
    [[ -z "$variants" ]] || echo "variants (candidate image): $variants"
    echo "contexts: $contexts (context limit $context_limit), cache $cache, transport $transport"
    echo "steady_rate_per_cpu: $steady_per_cpu; burst_factor: $burst_factor; steady_rates (KiB/s): ${steady_rates:-none}"
    echo "warmup $warmup s, steady $steady_window s, burst lead $burst_lead s, burst $burst_seconds s, recovery $recovery s"
    echo "adp cpuset $adp_cpuset, $regime_workers workers, $regime_decoders decoders; lading $lading_cpuset"
  } >"$out/series.txt"
  log "writing to $out"
  local -a order
  read -r -a order <<<"$labels"
  local count=${#order[@]} cpus round slot
  for cpus in $cpu_limits; do
    for ((round = 1; round <= rounds; round++)); do
      for ((slot = 0; slot < count; slot++)); do
        measure "$cpus" "${order[$(((slot + round - 1) % count))]}" "$round"
      done
    done
  done
  python3 -I "$here/summarize-regime.py" "$out" | tee "$out/summary.md"
}

regime_main "$@"
