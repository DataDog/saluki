# Compressed Ring Buffer Optimization Field Notes

## Baseline (current implementation)

- Encoding: musli packed → 4-byte BE length prefix → zstd level 11
- Config: max=2MiB, min_segment=256KiB
- Events retained: **68,708** / 500k (13.7%)
- Compression ratio: 6.87x
- Avg compressed bytes/event: 26.9
- Avg uncompressed bytes/event: ~186

### Byte breakdown per event (estimated, pre-compression)
- timestamp_nanos (u128): 16 bytes fixed
- level (&str): ~8 bytes (usize len prefix) + 4-5 bytes ("INFO"/"DEBUG"/"ERROR"/"WARN")
- target (&str): ~8 bytes len + ~30-45 bytes (module paths)
- message (String): ~8 bytes len + 10-60 bytes (varies)
- fields (Vec<u8>): ~8 bytes len + 0-100 bytes (varint-prefixed key-value pairs)
- file (Option<&str>): 0-40 bytes
- line (Option<u32>): 0-4 bytes
- **Framing**: 4 bytes per event (u32 BE length prefix)
- **Total**: ~100-200 bytes per event, averaging ~186 bytes

### Key observations
- usize length prefixes are 8 bytes on 64-bit, very wasteful for strings < 256 bytes
- timestamp is 16 bytes fixed but delta from previous is usually < 50ms = fits in 4 bytes
- level has only 5 possible values, stored as 4-5 byte string + 8 byte length prefix = 12-13 bytes for 3 bits of info
- target/file repeat from a small set, stored fully each time

---

## Trial 1: Delta-encode timestamps

**Hypothesis**: Timestamps are monotonically increasing with small deltas (1-50ms = 1M-50M nanos). Storing delta as u64 instead of absolute u128 saves 8 bytes/event. Storing as varint saves even more (1-4 bytes for typical deltas).

**Approach**: Store first timestamp as absolute u128 in the segment header. Subsequent events store delta from previous as varint-encoded nanoseconds.

**Result**: Combined into Trial 1 (below).

---

## Trial 1: Custom compact encoding (combined: delta timestamps, enum level, varint lengths, varint framing)

**Combined approach**: Replace musli serialization entirely with hand-rolled binary encoding:
- Delta-encode timestamps as varint (saves ~14 bytes/event: 16 → 2 bytes typical)
- Encode level as single u8 byte (saves ~12 bytes/event: 13 → 1 byte)
- Use varint length prefixes for strings/fields (saves ~6 bytes per field: 8 → 1-2 bytes)
- Use varint frame length instead of 4-byte u32 (saves ~2 bytes/event)
- Flags byte for optional file/line presence

**Result**: **79,391 events retained** (+15.5% over baseline 68,708)
- Compression ratio: 5.57x (down from 6.87x -- less redundancy for zstd to exploit)
- Avg compressed bytes/event: 22.7 (down from 26.9)
- Avg uncompressed bytes/event: ~130 (down from ~186)
- All 11 existing unit tests pass (round-trip encode/decode verified)

---

## Trial 2: String table for target/file deduplication (on top of Trial 1)

**Hypothesis**: target and file values repeat heavily across events in the same segment (~15 distinct targets, ~8 files). Storing a per-segment string table and using varint indices instead of full strings eliminates ~30-50 bytes of repeated string data per event.

**Approach**: Added `StringTable` struct to `EventBuffer`. On encode, target and file are interned and replaced with varint indices. On flush, the string table is serialized as a header before the event data. On decode, `CompressedSegmentReader` reads the string table first, stores ranges into the decompressed buffer, and resolves indices during event iteration.

**Result**: **89,896 events retained** (+30.8% over baseline, +13.2% over Trial 1)
- Compression ratio: 3.62x (down from 5.57x -- string dedup removes what zstd was compressing)
- Avg compressed bytes/event: 20.6 (down from 22.7)
- Segments: 20 live (fewer, larger segments since events are smaller)

---

## Trial 3: Parameter sweep (on top of Trials 1+2)

**Approach**: Sweep across compression levels and segment sizes with the new compact+interned encoding.

**Results** (sorted by events retained):
| Config                              | Retained | Rate  | Ratio | B/evt |
|-------------------------------------|----------|-------|-------|-------|
| 128k segments, zstd=19             | **93,891** | 18.8% | 3.60x | 21.1 |
| 64k segments, zstd=19              | 92,784 | 18.6% | 3.47x | 22.0 |
| 64k segments, zstd=11              | 90,950 | 18.2% | 3.42x | 22.4 |
| 256k segments, zstd=19             | 90,289 | 18.1% | 3.70x | 20.6 |
| 256k segments, zstd=11 (current)   | 89,896 | 18.0% | 3.62x | 20.6 |
| 32k segments, zstd=19              | 88,172 | 17.6% | 3.30x | 23.4 |
| 32k segments, zstd=11              | 87,388 | 17.5% | 3.26x | 23.7 |
| 256k segments, zstd=3              | 84,140 | 16.8% | 3.37x | 22.2 |
| 512k segments, zstd=11             | 77,075 | 15.4% | 3.68x | 18.7 |

**Key insight**: 128KiB segments + zstd=19 is the sweet spot. Smaller segments waste less buffer space but compress worse. Larger segments get better ratios but waste unflushed buffer space.

**Trade-off note**: zstd=19 is significantly slower than zstd=11 for CPU. This runs on a background thread so latency isn't critical, but CPU budget matters. Document for user decision.

---

## Trial 4: Intern field keys (on top of Trials 1+2)

**Hypothesis**: Field keys ("error", "listen_addr", "count", etc.) repeat across events. Currently stored inline in the fields blob. Interning them into the string table saves ~5-15 bytes per field per event.

**Approach**: In `encode_event`, parse the existing varint-length-prefixed fields blob, intern each key into the string table, and re-encode as `varint(num_fields) [varint(key_idx) varint(val_len) val ...]`. Decoder reconstructs the original format on read.

**Result**: **95,966 events retained** with default config (+39.7% over baseline)

**Best config with Trial 4** (from sweep): 128KiB segments + zstd=19 → **99,038 events** (+44.1% over baseline)

Full sweep results (sorted by events retained):
| Config                              | Retained | Rate  | Ratio | B/evt |
|-------------------------------------|----------|-------|-------|-------|
| 128k segments, zstd=19             | **99,038** | 19.8% | 3.25x | 20.0 |
| 64k segments, zstd=19              | 97,056 | 19.4% | 3.15x | 20.8 |
| 256k segments, zstd=19             | 96,908 | 19.4% | 3.32x | 19.2 |
| 64k segments, zstd=11              | 96,794 | 19.4% | 3.11x | 21.0 |
| 256k segments, zstd=11             | 95,966 | 19.2% | 3.28x | 19.4 |
| 32k segments, zstd=19              | 94,216 | 18.8% | 3.02x | 21.9 |
| 32k segments, zstd=11              | 93,246 | 18.6% | 2.99x | 22.2 |
| 256k segments, zstd=3              | 89,645 | 17.9% | 3.14x | 19.9 |
| 512k segments, zstd=11             | 88,607 | 17.7% | 3.34x | 18.3 |

---

## Trial 5: Intern field values (REJECTED)

**Hypothesis**: Interning field values (in addition to keys) into the string table.

**Result**: **89,777 events** -- WORSE than 95,966 without. Reverted.

**Why**: Most field values are unique (formatted numbers, specific error messages). Interning them bloats the string table without reducing event sizes. The string table overhead in the segment header exceeds savings. Compression ratio dropped to 2.48x.

---

## Trial 6: Hybrid message interning (REJECTED)

**Hypothesis**: Use a 0x00/0x01 tag to intern messages that have been seen before in the segment, while storing new messages inline.

**Result**: **92,883 events** -- WORSE than 95,966. Reverted.

**Why**: The 1-byte tag per event adds overhead even for unique messages (the majority). The string table growth from unique messages adds header overhead. zstd was already compressing repeated message patterns effectively.

---

## Trial 7: Default config tuning

**Approach**: Changed defaults from 256KiB segments + zstd=11 to 128KiB segments + zstd=19 based on Trial 3 sweep results.

**Result**: **99,038 events** (encoding from Trials 1+2+4, config from Trial 3 sweet spot)

**Trade-off**: zstd=19 uses more CPU than zstd=11 for compression. Runs on a dedicated background thread so it doesn't block the application, but increases CPU usage. The gains (+3,072 events over zstd=11 at same encoding, or ~3.2% more) may or may not justify the CPU cost depending on the deployment environment.

---

## Trial 8: Columnar storage layout

**Hypothesis**: Instead of row-oriented storage (all fields of event 1, then event 2, ...), store each field as a separate column. This groups similar data together for better compression, and enables specialized column encodings like RLE for repetitive fields.

**Segment layout**:
```
[string_table]
[varint(event_count)]
[col: timestamps]      -- varint-delta-encoded, length-prefixed blob
[col: levels]          -- RLE-encoded (varint run_len + varint value pairs)
[col: target_indices]  -- RLE-encoded
[col: messages]        -- varint-length-prefixed strings concatenated, length-prefixed blob
[col: fields]          -- per-event encoded fields concatenated, length-prefixed blob
[col: file_indices]    -- RLE-encoded (usize::MAX sentinel for None)
[col: lines]           -- varint-encoded (0=None, line+1=Some), length-prefixed blob
```

**Key improvements over row-oriented**:
- Levels column: 5 possible values → RLE compresses dramatically (e.g., 40 consecutive DEBUG events = 4 bytes instead of 40)
- Target indices: few distinct values, bursts from same module → RLE wins
- File indices: same as targets → RLE wins
- Timestamps: already delta-encoded, but now grouped together for even better zstd dictionary-like compression
- Messages/fields: variable-length high-entropy data is now cleanly separated from the compressible low-entropy data, so zstd can allocate its code space more efficiently

**Result**: **117,210 events retained** (+70.6% over original baseline, +18.3% over previous best row-oriented)

Sweep results:
| Config                              | Retained | Rate  | Ratio | B/evt |
|-------------------------------------|----------|-------|-------|-------|
| 128k segments, zstd=19 (default)   | **117,210** | 23.4% | 3.87x | 16.9 |
| 64k segments, zstd=19              | 115,099 | 23.0% | 3.71x | 17.7 |
| 256k segments, zstd=19             | 113,287 | 22.7% | 3.98x | 15.8 |
| 128k segments, zstd=11             | 108,856 | 21.8% | 3.55x | 18.2 |
| 128k segments, zstd=3              | 103,524 | 20.7% | 3.38x | 19.1 |

---

## Summary

| Stage | Events Retained | vs Baseline |
|-------|----------------|-------------|
| Original baseline (musli + 256KiB + zstd=11) | 68,708 | -- |
| Trial 1: Custom compact encoding | 79,391 | +15.5% |
| Trial 2: + String table dedup | 89,896 | +30.8% |
| Trial 4: + Field key interning | 95,966 | +39.7% |
| Trial 7: + Config tuning (128KiB + zstd=19) | 99,038 | +44.1% |
| Trial 8: Columnar layout | **117,210** | **+70.6%** |

### All optimizations applied:
1. Custom binary encoding (replaced musli)
2. Delta-encoded timestamps as varint
3. Level as u8 enum (1 byte)
4. Varint length prefixes for all strings
5. Per-segment string table for target paths, file paths, and field keys
6. **Columnar storage**: each field stored in its own column
7. **RLE encoding** for levels, target indices, and file indices columns
8. Config: 128KiB min segment size, zstd level 19

### Side effects:
- Removed `musli` dependency from `saluki-app`

### What didn't work:
- Interning field values: bloats string table, net negative (-6.4%)
- Hybrid message interning: per-event tag overhead exceeds savings (-3.2%)
- Very small segments (32/64KiB): compression ratio degrades too much
- Very large segments (512KiB): too much wasted unflushed buffer space

### Open trade-off: zstd compression level
zstd=19 uses more CPU than zstd=11 for compression. This runs on a dedicated background
thread and doesn't block the application. With columnar layout:
- zstd=19: 117,210 events (default)
- zstd=11: 108,856 events (-7.1%)
- zstd=3:  103,524 events (-11.7%)
If CPU budget is tight, zstd=11 is still +58.4% over the original baseline.

---

## Further Optimization Research (Web Search)

### Approach A: Bit-Packing for Integer Columns (Parquet/DuckDB-style)

**Source**: Apache Parquet DELTA_BINARY_PACKED encoding, DuckDB lightweight compression.

Our timestamps column is currently varint-delta-encoded. Parquet goes further with
DELTA_BINARY_PACKED: divide the delta column into blocks of N values, compute the
min-delta per block (Frame of Reference), subtract it out, then bit-pack the residuals
at the minimum required bit width per miniblock.

**Example**: If a block of 32 timestamp deltas are all in the range 25,000,000..26,000,000
(25-26ms), the FOR base is 25,000,000 and the residuals are 0..1,000,000 which fit in
20 bits. Instead of varint-encoding each delta (4 bytes), we store 20 bits each = 2.5 bytes.

**Applicability**: Our timestamp deltas are 1-50ms in nanoseconds (1M-50M), varint-encoded
as 4 bytes each. FOR+bit-packing could reduce this to 2-3 bytes, saving ~1-2 bytes/event.
Moderate gain, moderate complexity. The lines column (1-500) could also benefit.

**Key insight from DuckDB**: Bit-packing works per miniblock (e.g., 32 values), with a
separate bit-width per miniblock. This adapts to local value ranges rather than using
a global worst-case width.

### Approach B: Zstd Dictionary Compression

**Source**: Facebook zstd documentation, Cassandra CEP-54, OpenTelemetry collector issue #9707.

Train a zstd dictionary on representative uncompressed segment payloads and embed it as
a static constant. The dictionary provides "past data" that the compressor can reference
from the start of each segment. Benefits are most pronounced for the first few KB of data.

**Applicability**: Our segments are ~120-130KiB uncompressed. Zstd dictionaries help most
with small data (<32KiB). At our segment sizes, the compressor already has plenty of
context from earlier in the segment. Expected gain: small (maybe 2-5%).

**Implementation**: Train dictionary from first N segments' data using `zstd::dict::from_samples`,
embed as `static DICT: &[u8]`, use `EncoderDictionary::new(DICT, level)` and
`DecoderDictionary::new(DICT)`. The dictionary must be stored alongside the compressed
data or embedded in the binary.

**Trade-off**: Dictionary size (typically 32-112KiB) counts against the ring buffer memory
budget if stored in-process. An alternative is to train at startup and amortize across all
segments, but this adds complexity and the dictionary may not match the actual workload.

**Verdict**: Likely small gain for our segment sizes. Worth trying but unlikely to be a
top-tier optimization. Would be more impactful if we moved to smaller segments.

### Approach C: Log Template Extraction (LogShrink / LogFold-style)

**Source**: LogShrink (ICSE 2024), LogFold (2025), LogPrism (2025).

These academic systems achieve 16-356% better compression than general-purpose compressors
by exploiting the structure of log data:

1. **Template extraction**: Parse log messages to separate the static template ("Processed
   {} events in {}ms") from the variable parts ("1234", "56"). Store templates once in a
   dictionary; store only template ID + variable values per event.

2. **Variable matrix**: Group variables by template ID into a matrix (rows = events,
   columns = variable slots). Apply column-specific encoding: delta for monotonic counters,
   dictionary for repeated values, plain for high-entropy strings.

3. **Column-oriented storage**: LogShrink reports that column-oriented storage reduces
   compressed size by 36-103% compared to row-oriented storage (we already do this).

**Applicability**: High potential. Our messages column is currently the largest and highest-
entropy column. Many messages follow templates with repeated static text and a few variable
numeric values. Extracting templates would:
- Eliminate repeated static text entirely (stored once per template)
- Enable numeric-specific encoding for extracted variables
- Further reduce what zstd needs to compress

**Complexity**: Significant. Requires a log parser / template extractor, a template
dictionary, a variable encoding system, and corresponding decoder logic. The template
extractor could either be heuristic (simple pattern matching) or statistical.

**Simplified version**: Rather than full template extraction, we could:
- Separate messages into "message template ID" (interned) + "variable parts" columns
- Use a simple heuristic: split on whitespace, classify tokens as static (all alpha)
  vs variable (contains digits), intern the static skeleton

### Approach D: Cascaded Compression (BtrBlocks-style)

**Source**: BtrBlocks (SIGMOD 2023), SpiralDB.

BtrBlocks chains multiple lightweight encodings, each fast and preserving random access:
RLE → bit-packing → general compressor. The key insight is that **the output of one
encoding becomes the input to the next**.

**Applicability**: We already do RLE on levels/targets/files before zstd. We could extend
this to a cascade:
1. RLE on levels → produces (count, value) pairs
2. The counts column from RLE could itself be delta-encoded or bit-packed
3. The entire result then goes to zstd

For our data, the marginal gain of cascading on already-RLE'd data is likely small since
zstd is very good at compressing the resulting RLE output. More useful would be applying
bit-packing to the timestamp deltas and line numbers before zstd sees them.

### Approach E: Separate Compression per Column

**Source**: General columnar database design, Apache ORC.

Instead of compressing the entire segment payload as one zstd frame, compress each column
independently. This lets zstd build optimal Huffman tables and match finders for each
column's data distribution rather than trying to find a single strategy that works across
heterogeneous columns.

**Applicability**: Potentially high. Our payload mixes:
- Very low entropy: RLE-encoded levels (maybe 50 bytes)
- Medium entropy: delta-encoded timestamps, line numbers
- High entropy: messages, field values

Zstd's internal state is shared across all this data. Compressing each column separately
would let the compressor specialize. However, each separate zstd frame has ~18 bytes of
overhead (magic + frame header), so for very small columns (levels RLE = ~50 bytes) the
overhead could negate the gains.

**Hybrid approach**: Compress low-entropy columns (timestamps, levels, targets, files, lines)
together as one zstd frame, and high-entropy columns (messages, fields) as a separate frame.
This gives the compressor two distinct contexts without excessive frame overhead.

### Approach F: Field Value Column Splitting

**Source**: LogShrink variable matrix concept.

Our fields column currently stores all field key-value pairs sequentially. We could split
it into sub-columns:
- Field key indices column (already interned, small varint values)
- Field value bytes column (the actual values)
- Field count per event (small integer)

Separating the interned key indices from the raw value bytes would improve compression
since the key indices are very repetitive and the values are not.

### Approach G: Numeric Field Value Specialization

**Source**: "Unlocking the Power of Numbers: Log Compression via Numeric Token Parsing" (ASE 2024).

Many field values are numeric strings ("1234", "127.0.0.1", "56ms"). Parsing these into
binary integers/floats and storing them as typed columns would dramatically reduce size:
- "1234" (4 bytes UTF-8) → varint (2 bytes)
- "127.0.0.1" (11 bytes UTF-8) → 4 bytes (IPv4 packed)
- "1048576" (7 bytes UTF-8) → varint (3 bytes)

This requires type detection at encode time and is only applicable to field values, not
messages. The complexity is moderate but the per-event savings could be meaningful.

### Priority Assessment

| Approach | Expected Gain | Complexity | Risk |
|----------|--------------|------------|------|
| C: Template extraction (simplified) | High (10-25%) | High | Medium |
| E: Separate compression per column | Medium (5-15%) | Low | Low |
| F: Field value column splitting | Medium (5-10%) | Low | Low |
| A: Bit-packing for integers | Low-Med (3-8%) | Medium | Low |
| B: Zstd dictionary | Low (2-5%) | Low | Low |
| D: Cascaded compression | Low (1-3%) | Medium | Low |
| G: Numeric specialization | Low-Med (3-8%) | High | Medium |

---

## Trial E: Split compression (meta vs content frames)

**Approach**: Compress the segment as two separate zstd frames:
1. **Meta frame**: string table, event count, timestamps, levels, targets, files, lines, field counts, field key indices, message template indices (low-entropy, highly structured)
2. **Content frame**: message variables, field values (high-entropy, mostly unique text)

This lets zstd build optimal Huffman tables and match finders for each data distribution.

**Result**: **122,821 events** (+4.8% over single-frame columnar, +78.8% over original baseline)
- Compression ratio: 4.08x (up from 3.87x)
- Avg compressed bytes/event: 15.8

---

## Trial F: Field column splitting (on top of Trial E)

**Approach**: Split the fields column into three sub-columns:
- Field counts → `Vec<usize>` for RLE (meta frame)
- Field key indices → varint column (meta frame)
- Field values → varint-length-prefixed bytes (content frame)

**Result**: **130,366 events** (+6.1% over Trial E, +89.7% over original baseline)
- Compression ratio: 4.29x
- Avg compressed bytes/event: 15.2

---

## Trial C: Simplified message template extraction (on top of E+F)

**Approach**: Inspired by LogShrink (ICSE 2024). Split each message into a "skeleton"
(static tokens) and "variables" (tokens containing digits):
1. Tokenize message by whitespace
2. If a token contains any ASCII digit, replace it with `\x00` placeholder and store as variable
3. Intern the skeleton in the string table
4. Store skeleton index in meta (RLE-encoded), variable tokens in content

Example: "Processed 1234 events in 56ms" → skeleton "Processed \x00 events in \x00", variables ["1234", "56ms"]

**Result**: **137,259 events** (+5.3% over Trial F, **+99.8% over original baseline** -- 2x capacity)
- Compression ratio: 3.08x
- Avg compressed bytes/event: 14.5

Sweep results:
| Config                              | Retained | Rate  | Ratio | B/evt |
|-------------------------------------|----------|-------|-------|-------|
| 128k segments, zstd=19 (default)   | **137,259** | 27.5% | 3.08x | 14.5 |
| 64k segments, zstd=19              | 134,936 | 27.0% | 3.01x | 14.9 |
| 256k segments, zstd=19             | 133,807 | 26.8% | 3.15x | 13.9 |
| 128k segments, zstd=11             | 123,431 | 24.7% | 2.83x | 15.4 |
| 128k segments, zstd=3              | 119,098 | 23.8% | 2.72x | 16.4 |

---

## Trial A: Bit-packing / FOR encoding -- SKIPPED

**Rationale**: At 14.5 compressed bytes/event and 3.08x compression ratio, the timestamp
and line columns account for ~5-6 pre-compression bytes per event. Bit-packing would save
~1-2 bytes there, translating to ~0.3-0.6 compressed bytes after zstd -- roughly 2-4%.
The implementation complexity (miniblocks, bit-width tracking, bitstream packing) is
significant for a small gain. Skipped in favor of diminishing returns.

---

## Trial D: Drain-Inspired Pattern Clustering (on top of E+F+C)

**Hypothesis**: The current skeleton extraction only wildcards tokens containing
digits. Pure-word tokens that vary (hostnames, service names, endpoints like
"dogstatsd" vs "topology_runner") produce different skeletons. A Drain-inspired
clustering approach that learns to wildcard pure-word positions through
observation should produce fewer unique templates, improving RLE compression
on the template index column.

**Approach**: Replaced the naive SkeletonParser with a stateful ClusterManager
implementing the algorithm from the Datadog Agent log pattern extraction design
(sections 1-2). Key components:

1. **Typed tokenization**: Classify whitespace-delimited tokens as SeverityLevel
   (never wildcarded), Numeric (immediately wildcarded), or Word (potential wildcard).
2. **Signature-based clustering**: Hash the token-type sequence + first Word value
   (first-word protection). Messages with different first words or type sequences
   hash to different clusters.
3. **Pattern merging**: On match, compare token values. Same type + different value
   → wildcard. Different types → conflict (no merge). SeverityLevel never wildcards.
4. **Hot-pattern cache**: MRU entry per cluster for O(tokens) steady-state matching.
5. **Saturation scoring**: After 50 consecutive identical merges, skip the CanMerge
   pre-check (single O(tokens) pass instead of two).
6. **Callsite acceleration**: Use `&'static Metadata` pointer identity as a cache key.
   Three-tier: Learning (route to cached cluster), Converged (skip tokenization,
   extract wildcards by position), Unstable (skip cache entirely).

**Result**: **155,715 events** (+13.4% over Trial C, **+126.6% over original baseline**)
- Compression ratio: 3.48x (up from 3.08x)
- Avg compressed bytes/event: 12.9 (down from 14.5)

Sweep results:
| Config                              | Retained | Rate  | Ratio | B/evt |
|-------------------------------------|----------|-------|-------|-------|
| 128k segments, zstd=19 (default)   | **155,715** | 31.1% | 3.48x | 12.9 |
| 64k segments, zstd=19              | 152,737 | 30.5% | 3.39x | 13.2 |
| 256k segments, zstd=19             | 150,914 | 30.2% | 3.55x | 12.4 |
| 128k segments, zstd=11             | 141,364 | 28.3% | 3.18x | 14.1 |
| 128k segments, zstd=3              | 135,558 | 27.1% | 3.04x | 14.6 |

**Why it helps**: The benchmark generates 4 formatted message types where the
last argument comes from a static pool of 8 values (e.g., "dogstatsd",
"topology_runner", "/var/run/datadog/apm.socket"). The naive heuristic treats
non-digit values like "dogstatsd" as static text, producing 8 different skeletons
per message type where the Drain approach collapses them to 1. Fewer unique
templates → better RLE on the template index column → less metadata frame overhead.

---

## Final Summary

| Stage | Events Retained | vs Baseline |
|-------|----------------|-------------|
| Original baseline (musli + row-oriented) | 68,708 | -- |
| Custom compact encoding | 79,391 | +15.5% |
| + String table dedup | 89,896 | +30.8% |
| + Field key interning | 95,966 | +39.7% |
| + Config tuning (128KiB + zstd=19) | 99,038 | +44.1% |
| + Columnar layout + RLE | 117,210 | +70.6% |
| + Split compression (meta/content frames) | 122,821 | +78.8% |
| + Field column splitting | 130,366 | +89.7% |
| + Message template extraction (naive) | 137,259 | +99.8% |
| + Drain-inspired pattern clustering | **155,715** | **+126.6%** |

---

# Stability Optimization Hunt

Previous optimizations focused on **peak retained events**: how many events the ring buffer can
hold at its fullest. In practice, what matters to log-recovery usefulness is the *time range*
of events the buffer covers -- i.e. the **minimum** retained count as the buffer cycles. A
configuration that peaks at 155k events but dips to 112k moments later has a worse floor than
one that peaks at 152k but never dips below 150k.

This section tracks trials that target the **minimum** retained-event count and the delta
between min and average, rather than the peak.

## Methodology: stability benchmark

Added `ring_buffer_stability_bench` / `ring_buffer_stability_sweep` in `benchmarks.rs`. Both
feed 750k--1.5M synthetic events (~10 hours simulated log time at ~25ms/event) and sample
retained count after **every** event add. Two phases are reported:

- **Early**: samples start after the 1st segment drop (first time eviction occurs).
- **Steady**: samples start after the 50th drop (startup transients fully flushed).

For each phase the benchmark computes min / p1 / p10 / p50 / avg / p90 / p99 / max of
retained events, plus drop-amplitude distribution (events lost in a single step) and
coverage duration (oldest retained event to newest).

## Baseline (post-capacity-optimization, pre-stability-optimization)

Pre-Trial-I behavior (the "grow segments under no pressure" flush gate) at default config
(max=2MiB, min_segment=128KiB, zstd=19), 1.5M events:

**Early phase (from first drop)**:
- min retained: **112,207**
- avg retained: 152,747
- max-min delta: 50,655 (33.2% of avg)
- min as % of avg: **73.5%**
- max drop amplitude: 36,633 events (single step loss)

**Steady phase (after 50 drops)**:
- min retained: 150,034
- avg retained: 153,781
- max-min delta: 6,413 (4.2% of avg)
- min as % of avg: 97.6%
- max drop amplitude: 5,605 events

**Root cause of the gap between early and steady phase**: during startup, the event buffer
is allowed to grow far beyond `min_segment_size_bytes` because the gate requires
`total_size_bytes > max_ring_buffer_size_bytes`, which doesn't trip until the whole budget
is nearly full. The first segment is compressed from up to 2 MiB of uncompressed data, the
second from ~1.4 MiB, and so on -- a handful of "monster" segments 3--8x normal size get
created. When these monster segments later reach the eviction queue, each eviction drops
tens of thousands of events at once. By the time all monster segments have been evicted
(~50 drops), the behavior stabilizes, but until then the min retention is horrible.

---

## Trial I: Flush at `min_segment_size_bytes` regardless of memory pressure

**Hypothesis**: Remove the `total_size_bytes > max` gate on the flush path so that segments
are capped at `min_segment_size_bytes` bytes from the very first flush. This eliminates the
startup "monster segments" and should make eviction amplitudes uniform at all times.

**Change**: In `ProcessorState::add_event`, replace

```rust
if self.total_size_bytes() > self.config.max_ring_buffer_size_bytes
    && self.event_buffer.size_bytes() >= self.config.min_uncompressed_segment_size_bytes
{
    let compressed_segment = self.event_buffer.flush()?;
    // ...
}
```

with

```rust
if self.event_buffer.size_bytes() >= self.config.min_uncompressed_segment_size_bytes {
    let compressed_segment = self.event_buffer.flush()?;
    // ...
}
```

**Result** (default 128KiB, zstd=19):

**Early phase (from first drop)**:
- min retained: **150,246** (up from 112,207 -- **+33.9%** in minimum retention)
- avg retained: 152,368 (down from 152,747, -0.2%)
- max-min delta: 4,342 (down from 50,655, **-91%**)
- min as % of avg: **98.6%** (up from 73.5%)
- max drop amplitude: 3,411 (down from 36,633, **-90.7%**)

**Steady phase**: identical to early phase -- behavior is now **fully stable from the first
drop onward**, no warmup required.

**Cost**: peak retention drops slightly. The capacity benchmark (500k events) reports
151,682 retained (down from 155,715, -2.6%). Pre-compression segment size is now capped at
`min_segment_size_bytes` whereas it used to be allowed to grow to the full budget; zstd
gets marginally less context per frame, costing ~7% on bytes/event (12.9 → 13.0). This is a
worthwhile trade since minimum retention is what drives usefulness.

### Sweep across segment sizes and compression levels (Trial I applied), 750k events

| Config             | min    | avg    | max-min | min/avg | max drop |
|--------------------|--------|--------|---------|---------|----------|
| 128k, zstd=19      |**150,246**|**152,200**| 3,922 | 98.7% | 3,411 |
| 128k, zstd=22      | 150,246| 152,200| 3,922   | 98.7%   | 3,411    |
| 128k, zstd=11      | 137,349| 139,281| 3,860   | 98.6%   | 3,411    |
| 64k, zstd=19       | 147,727| 149,399| 3,010   | 98.9%   | 1,715    |
| 32k, zstd=19       | 139,632| 140,467| 1,567   | 99.4%   | 893      |
| 256k, zstd=19      | 143,917| 147,424| 7,059   | 97.6%   | 6,662    |

**Key takeaways**:

- **128k + zstd=19 remains the best config** for absolute minimum retention.
- **zstd=22 gives no gain over zstd=19** at these segment sizes (zstd's max level hits its
  ceiling long before segment boundaries become the bottleneck). zstd=11 costs ~13k events.
- **Smaller segments reduce drop amplitude but not delta/avg ratio** -- 32k drops are 74%
  smaller than 128k drops, but the absolute minimum is also 10k events lower, so 128k still
  wins on what the user cares about most. The "drop amplitude" alone is a misleading metric;
  it needs to be weighed against absolute retention.
- **256k segments lose on both axes** -- they suffer the same marginal compression-ratio
  improvement seen pre-Trial-I but now also incur doubled drop amplitude, because with
  monster-segment elimination gone, there's no scenario where "grow larger" pays off.

**Why max-min delta is floor-limited to ~segment_size_in_events**: at steady state, each
cycle consists of (many events added, one segment worth dropped). Retention oscillates by
one segment's worth plus some noise from variability in event-add rate between drops.
Further reducing delta requires **sub-segment eviction granularity**, which in turn
requires either partial segment decompression/re-encoding (high CPU cost) or zstd
dictionary compression so small frames share context (substantial complexity). Neither is
obviously worth the marginal gain -- the current 2.6% delta/avg is close to what
whole-segment eviction allows.

### Stability summary

| Metric (at 128k segments) | Baseline (early) | Trial I | Change |
|---------------------------|------------------|---------|--------|
| Min retained              | 112,207          | 150,246 | +33.9% |
| Avg retained              | 152,747          | 152,368 | -0.2%  |
| Max-min delta             | 50,655           | 4,342   | -91.4% |
| Min / avg ratio           | 73.5%            | 98.6%   | +25 pp |
| Max drop amplitude        | 36,633           | 3,411   | -90.7% |
| Min coverage duration     | 47.6m            | 63.8m   | +34%   |

The optimization is almost free at steady state and massively improves transient behavior.
