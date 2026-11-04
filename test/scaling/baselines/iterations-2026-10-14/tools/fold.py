# Folds `perf script --no-inline -F comm,tid,period,ip,sym` output; see ../README.md. Scratch analysis, kept for reproducibility.
import re, sys
from collections import Counter
path = sys.argv[1]
pattern = re.compile(sys.argv[2]) if len(sys.argv) > 2 else None
samples, cur = [], None
for line in open(path):
    if not line.startswith(('\t', ' ')) and line.strip():
        cur = {'comm': line.split()[0], 'frames': []}
        samples.append(cur)
    elif line.strip() and cur is not None:
        parts = line.strip().split(' ', 1)
        sym = parts[1] if len(parts) > 1 else '?'
        sym = re.sub(r'::h[0-9a-f]{16}$', '', sym)
        sym = re.sub(r'\+0x[0-9a-f]+$', '', sym)
        cur['frames'].append(sym)
total = len(samples)
inclusive = Counter()
for s in samples:
    for sym in set(s['frames']):
        inclusive[sym] += 1
print(f'{total} samples')
shown = 0
for sym, n in inclusive.most_common():
    if pattern and not pattern.search(sym):
        continue
    print(f'{100 * n / total:6.2f}%  {sym[:190]}')
    shown += 1
    if shown >= int(sys.argv[3] if len(sys.argv) > 3 else 40):
        break
