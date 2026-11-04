"""From a run's kept scrapes (SCALING_KEEP_SCRAPES=1), the counts over the window: series aggregation flushed,
flushes, events into the metrics encoder, metrics decoded, and series per flush.

Usage: python3 -I outputs.py <scrapes dir>..."""
import os, re, sys

def read(path):
    vals = {}
    for line in open(path):
        if line.startswith('#') or not line.strip():
            continue
        k, _, v = line.rpartition(' ')
        try:
            vals[k] = float(v)
        except ValueError:
            pass
    return vals

def total(vals, pat):
    return sum(v for k, v in vals.items() if re.search(pat, k))

PATTERNS = {'flushed series': r'^adp__aggregate_flushed_total', 'flushes': r'^adp__aggregate_flushes_total',
            'encoder in': r'^adp__component_events_received_total\{.*component_id="dd_metrics_encode"',
            'decoded': r'^adp__component_events_received_total\{.*component_id="dsd_in".*message_type="metrics"'}

for d in sys.argv[1:]:
    files = sorted(os.listdir(d))
    a, b = read(os.path.join(d, files[0])), read(os.path.join(d, files[-1]))
    span = float(files[-1][:-5]) - float(files[0][:-5])
    out = {k: total(b, p) - total(a, p) for k, p in PATTERNS.items()}
    per = out['flushed series'] / out['flushes'] if out['flushes'] else float('nan')
    print(f"{os.path.basename(d):34} {span:5.0f}s  " + '  '.join(f'{k} {v:,.0f}' for k, v in out.items()) + f'  series/flush {per:,.0f}')
