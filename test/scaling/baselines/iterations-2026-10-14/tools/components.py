# Folds `perf script --no-inline -F comm,tid,period,ip,sym` output; see ../README.md. Scratch analysis, kept for reproducibility.
import re, sys
from collections import Counter
cats = [
    ('allocator (jemalloc, tracking)', r'_rjem_|jemalloc|TrackingAllocator|GlobalAlloc|__rust_alloc|__rust_dealloc|malloc|free$'),
    ('hashing', r'hash|Hash|xxh|ahash|foldhash|SipHash'),
    ('compression', r'zstd|ZSTD|deflate|flate|zlib|compress'),
    ('memcpy/memmove/memcmp', r'^mem(cpy|move|cmp|set)|__memcpy|__memmove'),
    ('kernel', r'^\[k\]|^[a-z_0-9.]+$'),
    ('context resolution / interning', r'ContextResolver|resolve_with|Interner|intern|TagsResolver|FrontCache'),
    ('DogStatsD decode', r'dogstatsd|handle_frame|parse_|codec|Decoder|decode'),
    ('aggregation', r'[Aa]ggregat'),
    ('plan runtime (regions, kernels, decisions)', r'saluki_metric_plan|saluki_metric_kernels|region::|RegionHost|RegionDriver|Lane'),
    ('encoder', r'[Ee]ncode|request_builder|RequestBuilder|protobuf|proto'),
    ('forwarder / HTTP', r'[Ff]orwarder|hyper|http|Delivery'),
    ('interconnect / dispatch', r'interconnect|dispatch|Dispatch|EventsBuffer|FixedSizeEventBuffer'),
    ('internal telemetry', r'observability|metrics::|reflector'),
    ('tokio runtime', r'tokio'),
]
def load(path):
    samples, cur = [], None
    for line in open(path):
        if not line.startswith(('\t', ' ')) and line.strip():
            cur = []; samples.append(cur)
        elif line.strip() and cur is not None:
            parts = line.strip().split(' ', 1)
            cur.append(parts[1] if len(parts) > 1 else '?')
    return samples
def classify(frames):
    for f in frames:  # leaf first
        if f == '[unknown]':
            continue
        for name, pat in cats:
            if re.search(pat, f):
                return name
    return 'other'
res = {}
for path in sys.argv[1:]:
    s = load(path)
    c = Counter(classify(f) for f in s)
    res[path] = (len(s), c)
names = [n for n, _ in cats] + ['other']
print('component'.ljust(46) + ''.join(p.split('-')[-1][:-4].rjust(10) for p in sys.argv[1:]))
for n in names:
    print(n.ljust(46) + ''.join(f'{100*res[p][1][n]/res[p][0]:9.1f}%' for p in sys.argv[1:]))
print('samples'.ljust(46) + ''.join(f'{res[p][0]:10d}' for p in sys.argv[1:]))
