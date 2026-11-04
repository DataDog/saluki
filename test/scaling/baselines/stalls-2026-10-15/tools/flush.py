"""Per-flush effects, from a run's host samples (fast-*.csv) and, for the plan, its flow trace (flow-*.txt).

Anchors each flush at its drain (plan), or at the CPU peak of each 15-second window (main, which has no trace; this
anchoring proved unreliable, see ../README.md). Prints, per run, the mean over flushes of the drain's duration and
output, the extra CPU in [0, 3 s) above the second before, the extra throttled time (summed over CPUs) in [0, 3 s)
against the 3 s before, the share of samples with datagrams waiting at the socket before and after, and the admission
deficit over 3 s (plan). `print_profiles` prints a 100 ms profile around the anchor.

Usage: python3 -I flush.py <run dir> <run name>..."""
import csv, statistics, sys
from collections import defaultdict

FLUSH = 15.0

def fast_rows(path):
    rows = list(csv.DictReader(open(path)))
    out = []
    for a, b in zip(rows, rows[1:]):
        try:
            dt = float(b['t']) - float(a['t'])
            if dt <= 0:
                continue
            out.append({'t': float(b['t']), 'dt': dt,
                        'cores': (int(b['cpu_usec']) - int(a['cpu_usec'])) / 1e6 / dt,
                        'thr_ms': (int(b['throttled_usec']) - int(a['throttled_usec'])) / 1000,
                        'waiting': 1.0 if b['socket_queued'] not in ('-', '', '0') else 0.0})
        except (ValueError, KeyError):
            pass
    return out

def flow(path):
    drains, admitted = [], []
    try:
        for line in open(path):
            p = line.split()
            if p and p[0] == 'd' and p[2].startswith('dsd_agg'):
                drains.append((int(p[1]) / 1000, int(p[3]) / 1000, int(p[6])))
            elif p and p[0] == 'w' and p[2] == 'dsd_in.metrics':
                admitted.append((int(p[1]) / 1000, int(p[5])))
    except FileNotFoundError:
        pass
    return drains, admitted

def window_sum(rows, key, lo, hi):
    return sum(r[key] * (r['dt'] if key == 'cores' else 1) for r in rows if lo <= r['t'] < hi)

def analyze(run_dir, name):
    rows = fast_rows(f'{run_dir}/fast-{name}.csv')
    t0, t1 = rows[0]['t'], rows[-1]['t']
    drains, admitted = flow(f'{run_dir}/flow-{name}.txt')
    if drains:
        anchors = [d[0] for d in drains if t0 + 3 <= d[0] <= t1 - 4]
    else:
        anchors, t = [], t0
        while t + FLUSH <= t1:
            win = [r for r in rows if t <= r['t'] < t + FLUSH]
            peak = max(win, key=lambda r: r['cores'])['t']
            if t0 + 3 <= peak <= t1 - 4:
                anchors.append(peak)
            t += FLUSH
    res = {'name': name, 'flushes': len(anchors)}
    extra, thr, deficit, wait_after, wait_before = [], [], [], [], []
    for a in anchors:
        extra.append(window_sum(rows, 'cores', a, a + 3) - 3 * window_sum(rows, 'cores', a - 1, a))
        thr.append(window_sum(rows, 'thr_ms', a, a + 3) - window_sum(rows, 'thr_ms', a - 3, a))
        wa = [r['waiting'] for r in rows if a <= r['t'] < a + 1]
        wb = [r['waiting'] for r in rows if a - 3 <= r['t'] < a - 0.5]
        if wa and wb:
            wait_after.append(statistics.mean(wa)); wait_before.append(statistics.mean(wb))
        if admitted:
            before = sum(i for t, i in admitted if a - 3 <= t < a)
            after = sum(i for t, i in admitted if a <= t < a + 3)
            deficit.append((before - after) / before if before else 0)
    mean = lambda v: statistics.mean(v) if v else None
    res.update(extra_core_s=mean(extra), extra_thr_ms=mean(thr), wait_after=mean(wait_after),
               wait_before=mean(wait_before), deficit=mean(deficit),
               drain_ms=mean([d[1] for d in drains]), drain_events=mean([d[2] for d in drains]))
    prof = defaultdict(lambda: defaultdict(list))
    for a in anchors:
        for r in rows:
            k = round((r['t'] - a) / 0.1)
            if -10 <= k < 40:
                prof[k]['cores'].append(r['cores']); prof[k]['thr'].append(r['thr_ms']); prof[k]['wait'].append(r['waiting'])
        for t, i in admitted:
            k = round((t - a) / 0.1)
            if -10 <= k < 40:
                prof[k]['adm'].append(i)
    res['profile'] = {k: {m: statistics.mean(v) for m, v in d.items()} for k, d in prof.items()}
    return res

def print_profiles(run_dir, names, lo=-10, hi=40):
    rs = [analyze(run_dir, n) for n in names]
    print('t(ms)'.rjust(6) + ''.join(f"{r['name'][-14:]:>34}" for r in rs))
    print(' ' * 6 + ''.join(f"{'cores':>8}{'thr ms':>8}{'wait':>6}{'admit':>12}" for _ in rs))
    for k in range(lo, hi):
        line = f'{k * 100:6d}'
        for r in rs:
            p = r['profile'].get(k, {})
            g = lambda m, s, w: (format(p[m], s) if m in p else '-').rjust(w)
            line += g('cores', '.2f', 8) + g('thr', '.0f', 8) + g('wait', '.2f', 6) + g('adm', '.0f', 12)
        print(line)

if __name__ == '__main__':
    for name in sys.argv[2:]:
        r = analyze(sys.argv[1], name)
        f = lambda v, s: '-' if v is None else format(v, s)
        print(f"{name:28} flushes {r['flushes']:2}  drain {f(r['drain_ms'], '.0f')} ms ({f(r['drain_events'], '.0f')} out)  "
              f"extra CPU {f(r['extra_core_s'], '.3f')} core-s  extra throttled {f(r['extra_thr_ms'], '.0f')} ms  "
              f"socket waiting {f(r['wait_before'], '.2f')} -> {f(r['wait_after'], '.2f')}  admission deficit {f(r['deficit'], '.2f')}")
