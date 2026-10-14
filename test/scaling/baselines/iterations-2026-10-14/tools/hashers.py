# Folds `perf script --no-inline -F comm,tid,period,ip,sym` output; see ../README.md. Scratch analysis, kept for reproducibility.
import re, sys
from collections import Counter
samples, cur = [], None
for line in open(sys.argv[1]):
    if not line.startswith(('\t', ' ')) and line.strip():
        cur = []; samples.append(cur)
    elif line.strip() and cur is not None:
        parts = line.strip().split(' ', 1)
        cur.append(re.sub(r'::h[0-9a-f]{16}$', '', parts[1] if len(parts) > 1 else '?'))
leaf, caller = Counter(), Counter()
n = 0
for s in samples:
    frames = [f for f in s if f != '[unknown]']
    if not frames or not re.search(r'hash|Hash|xxh|ahash|foldhash|SipHash', frames[0]):
        continue
    n += 1
    leaf[frames[0][:110]] += 1
    for f in frames[1:]:
        if not re.search(r'hash|Hash|xxh|ahash|foldhash|SipHash|core::|alloc::|<.*as core', f):
            caller[f[:150]] += 1
            break
total = len(samples)
print(f'hashing leaf samples: {n} of {total} ({100*n/total:.1f}%)')
print('-- leaf functions'); [print(f'{100*c/total:5.1f}%  {k}') for k, c in leaf.most_common(6)]
print('-- first non-hash caller'); [print(f'{100*c/total:5.1f}%  {k}') for k, c in caller.most_common(8)]
