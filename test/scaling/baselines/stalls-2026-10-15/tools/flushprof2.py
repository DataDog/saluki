"""Flush cost by owning component, from timestamped instruction samples.

Input: `perf script --no-inline -F comm,tid,time,period,ip,sym` output of a run recorded with `perf record -k mono`,
the run's `perf-<run>.offset` (wall clock minus monotonic clock), and, for the plan, its flow trace. Anchors: the
plan's drains, or, for main, the first encoder sample after a quiet gap of 5 s. Each sample goes to the component
owning its stack (the first match, by priority, over all frames), and a flush's added instructions are (rate in
[0, 3 s) after the anchor - steady rate away from flushes) x 3 s, per flush.

Usage: python3 -I flushprof2.py <perf script output> <offset file> [<flow trace>]"""
import re, sys
from collections import Counter

OWNERS = [
    ('encoder (incl. compression)', r'encoders::datadog|encoders\.\.datadog|MetricsEncodeHost|request_builder|flush_v2_metrics'),
    ('forwarder / HTTP', r'forwarders|DeliveryHost|hyper::|reqwest'),
    ('aggregation', r'AggregateKernel|aggregate::kernel|transforms\.\.aggregate\.\.Aggregate|transforms::aggregate'),
    ('post-aggregation transforms', r'host_enrichment|HostEnrichment|host_tags|HostTags|post_aggregate|PostAggregate'),
    ('plan regions (other)', r'MetricRegionDriver|saluki_metric_plan|region::host|region\.\.host'),
    ('DogStatsD source', r'dogstatsd|DogStatsD|handle_frame'),
    ('internal telemetry', r'observability::metrics|reflector'),
    ('tokio / kernel / other', r'.'),
]
COMPRESSION = r'zstd|ZSTD|async_compression|deflate|flate'

def samples(path, offset):
    out, cur = [], None
    for line in open(path):
        if not line.startswith(('\t', ' ')) and line.strip():
            m = re.search(r'\s([0-9]+\.[0-9]+):\s+([0-9]+)?', line)
            cur = {'t': float(m.group(1)) + offset if m else None, 'w': int(m.group(2)) if m and m.group(2) else 100000, 'f': []}
            out.append(cur)
        elif line.strip() and cur is not None:
            parts = line.strip().split(' ', 1)
            cur['f'].append(parts[1] if len(parts) > 1 else '?')
    return [s for s in out if s['t'] is not None]

def owner(frames):
    joined = '\n'.join(frames)
    for name, pat in OWNERS:
        if re.search(pat, joined):
            return name

def main(prof, offset_file, flow=None, span=3.0):
    ss = samples(prof, float(open(offset_file).read()))
    t0, t1 = ss[0]['t'], ss[-1]['t']
    if flow:
        anchors = [int(l.split()[1]) / 1000 for l in open(flow) if l.startswith('d ') and l.split()[2].startswith('dsd_agg')]
    else:
        anchors, last = [], -1e9
        for s in ss:
            if owner(s['f']) == OWNERS[0][0]:
                if s['t'] - last > 5:
                    anchors.append(s['t'])
                last = s['t']
    anchors = [a for a in anchors if t0 + 1 <= a <= t1 - span]
    onsets = []
    for a in anchors:
        enc = [s['t'] for s in ss if a <= s['t'] < a + span and owner(s['f']) == OWNERS[0][0]]
        if enc:
            onsets.append((enc[0] - a, enc[-1] - a))
    inside = lambda t: any(a <= t < a + span for a in anchors)
    near = lambda t: any(a - 1 <= t < a + span + 1 for a in anchors)
    after, steady, comp_after = Counter(), Counter(), 0
    for s in ss:
        o = owner(s['f'])
        if inside(s['t']):
            after[o] += s['w']
            if o == OWNERS[0][0] and re.search(COMPRESSION, '\n'.join(s['f'][:6])):
                comp_after += s['w']
        elif not near(s['t']):
            steady[o] += s['w']
    t_after = span * len(anchors)
    t_steady = (t1 - t0) - (span + 2) * len(anchors)
    print(f'{len(anchors)} flushes' + (f'; encoder active from +{min(o[0] for o in onsets):.2f}s to +{max(o[1] for o in onsets):.2f}s after the anchor' if onsets else ''))
    rows, tot = [], 0
    for name, _ in OWNERS:
        ra, rs = after[name] / t_after, steady[name] / t_steady if t_steady > 0 else 0
        rows.append((name, ra, rs, (ra - rs) * span)); tot += (ra - rs) * span
    print(f"{'owner':32}{'after Minst/s':>14}{'steady Minst/s':>15}{'extra Minst/flush':>19}{'share':>7}")
    for name, ra, rs, ex in rows:
        print(f'{name:32}{ra/1e6:14.0f}{rs/1e6:15.0f}{ex/1e6:19.0f}{ex/tot if tot else 0:7.0%}')
    print(f"{'total':32}{sum(r[1] for r in rows)/1e6:14.0f}{sum(r[2] for r in rows)/1e6:15.0f}{tot/1e6:19.0f}")
    print(f'  compression (leaf frames, inside the encoder) after the flush: {comp_after / len(anchors) / 1e6:.0f} Minst/flush')

if __name__ == '__main__':
    main(*sys.argv[1:])
