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

---

# Fresh Optimization Round (2026-07)

A from-scratch re-evaluation of the full implementation, driven by a **per-column byte breakdown**
rather than intuition. The prior rounds optimized message structure and column splitting, but never
measured where the compressed bytes actually landed once all of that was in place.

## Methodology: per-column breakdown

Added `EventBuffer::column_breakdown()` (test-only) and the `ring_buffer_column_breakdown` benchmark.
It fills one ~128 KiB segment, then reports, per column: uncompressed bytes, bytes when compressed
*in isolation* at the configured level, and share of the total. Compressing each column alone
overstates absolute sizes (no shared zstd context) but gives a reliable *relative* ranking.

### Breakdown at the pre-round baseline (ns timestamps, 151,682 cap)

| Column | compressed b/evt | % of total |
|--------|-----------------:|-----------:|
| **timestamps** | 3.66 | **29.1%** |
| **field_values** | 3.34 | 26.6% |
| **msg_variables** | 2.52 | 20.1% |
| target_indices | 0.69 | 5.5% |
| msg_template_indices | 0.69 | 5.5% |
| field_key_indices | 0.56 | 4.4% |
| field_counts | 0.40 | 3.2% |
| levels | 0.33 | 2.6% |
| file_indices | 0.21 | 1.7% |
| string_table | 0.17 | 1.3% |
| lines | 0.01 | 0.0% |

**Headline finding**: the timestamps column was the single largest cost (29%) and compressed at only
**1.09x** -- the delta varints are uniform-random nanosecond values (1-50 ms inter-event) and thus
near-incompressible. Trial A had dismissed timestamp work as a 2-4% lossless bit-packing gain. That
analysis missed the far bigger, *lossy* lever: **precision**.

## Trial T1/T2: reduce timestamp precision (quantize deltas)

**Hypothesis**: Debug-log post-mortem does not need nanosecond timestamps. Quantizing each event's
timestamp to a coarser unit before delta-encoding shrinks the delta from a 4-byte high-entropy varint
to a 1-byte value, and the smaller values also compress better.

**Approach**: Added `TIMESTAMP_GRANULARITY_NS`. Encoder computes `ts_units = ts_nanos / GRAN`,
delta-encodes `ts_units`; decoder reconstructs `ts_units` and scales back (`* GRAN`). Both sides work
on integer units, so reconstruction is **drift-free** (rounding does not accumulate across a segment).
Sub-granularity precision is discarded; event *ordering* is preserved (events are stored in arrival
order regardless).

**Result** (default config, 500k events):

| Granularity | Retained | b/evt | vs ns baseline |
|-------------|---------:|------:|---------------:|
| ns (1) -- original | 151,682 | 13.0 | -- |
| µs (1,000) | 170,586 | 11.7 | +12.5% |
| **ms (1,000,000)** | **198,268** | **9.9** | **+30.7%** |

Chose **ms** as the default. The timestamps column drops from 3.66 → 0.73 compressed b/evt (29% →
7.6% of total). This is the largest single-optimization gain in the project's history and was
entirely missed by prior rounds.

**Trade-off flagged for the user**: this is lossy. Two events <1 ms apart may reconstruct to the same
wall-clock millisecond (their relative order is still preserved by storage order). ms is the standard
resolution for human-facing log output, so this is expected to be fine, but the granularity is a
one-line constant (`TIMESTAMP_GRANULARITY_NS`) and could be promoted to `RingBufferConfig` if any
consumer needs finer resolution. If it becomes configurable, the granularity must be written into the
segment header so the decoder can scale correctly.

### Breakdown after T1 (ms timestamps) -- new priority ranking

| Column | compressed b/evt | % of total |
|--------|-----------------:|-----------:|
| **field_values** | 3.34 | **34.7%** |
| **msg_variables** | 2.53 | 26.2% |
| timestamps | 0.73 | 7.6% |
| target_indices | 0.69 | 7.2% |
| msg_template_indices | 0.69 | 7.2% |
| field_key_indices | 0.56 | 5.8% |
| field_counts | 0.40 | 4.1% |
| levels | 0.32 | 3.4% |
| file_indices | 0.20 | 2.1% |
| string_table | 0.16 | 1.6% |
| lines | 0.01 | 0.1% |

The two high-entropy content columns (`field_values` + `msg_variables`) are now **61%** of the
compressed footprint. That is where the next rounds must focus.

## Trial V: drop the redundant per-event message variable count

**Hypothesis**: The content frame stored `varint(var_count)` before each event's variable tokens, but
`var_count` is fully determined by the number of `\0` placeholders in that event's template skeleton
(which the decoder already has). It is pure redundancy.

**Approach**: Stop writing the count on encode. On decode, compute
`var_count = template.bytes().filter(|&b| b == 0).count()`.

**Result**: **204,434 events** (+3.1% over T1). Larger than the naive "1 byte/event" estimate because
removing the interleaved count also de-fragments the token byte stream, which zstd likes.

**Safety note**: This relies on the existing invariant that message text never contains a literal
`\0` (already assumed -- the skeleton uses `\0` as its placeholder delimiter, and reconstruction
splits on it).

## Trial S: split message variable tokens into length + byte columns (TRIED, then REVERTED)

**Hypothesis**: `msg_variables` interleaved token lengths (low-entropy small ints) with token bytes
(high-entropy text). Trial F showed splitting the *field* column this way helped; the message column
had never been split the same way.

**Approach**: Two columns -- `msg_var_lens` (varint lengths, meta frame) and `msg_var_bytes`
(concatenated token bytes, content frame).

**Result**: **204,434 events** -- flat on retained count. The isolated message-variable cost fell
slightly (2.53 → 2.21 combined b/evt), but on the real combined frame the effect was within noise.
**Reverted** after Trial FV (below) showed the analogous field split was an outright regression: the
same co-location mechanism that hurt field values applies to any column with repeated verbatim values,
and real message variables (hostnames, service names, endpoints) repeat verbatim even though this
synthetic generator's variables are mostly random numbers. Kept the codebase on the simpler,
production-safer single-column form; zero measured benchmark cost (still 213,298 with Trial CS).

> **Measurement note**: the capacity benchmark's integer "events retained" is quantized by whole-
> segment eviction (±~4k events ≈ ±2%), so sub-2% wins are invisible there. From here on, the
> **real combined-frame `bytes/event` from `ring_buffer_column_breakdown`** is used as the sensitive
> primary metric, with capacity retained as confirmation.

## Trial FV: split field values into length + byte columns (REJECTED -- regression)

**Hypothesis**: Same as Trial S, applied to `field_values` (the single largest column, ~37%).

**Approach**: `field_value_lens` (varint lengths, meta) + `field_values` (pure bytes, content).

**Result**: **209,392 events (-1.8%)**; real b/evt 9.21 → 9.43. The field column's *combined* isolated
cost rose 3.34 → 3.52. **Rejected and reverted.**

**Why it hurt (important general lesson)**: field values have strong length↔value correlation because
many are fixed-length strings drawn from a small pool (e.g. `"dogstatsd"` is always 9 bytes). With the
length co-located, zstd matches the whole `\x09dogstatsd` sequence as one repeated unit. Splitting the
length into a separate column breaks that match and scatters the (otherwise very compressible) length
byte into a stream mixed with unrelated lengths. **Takeaway: length/byte splitting only helps when
values do NOT repeat verbatim (random numbers); it hurts whenever they do. Since verbatim repetition
is the common, compressible case in real logs, do not split length from bytes for text columns.**

## Trial CS: callsite interning (collapse target/file/line/level)

**Hypothesis**: `target`, `file`, `line`, and `level` are *all* fixed properties of a log callsite
(the `&'static tracing::Metadata`). Encoding them as four independent per-event columns re-derives,
per event, information that is constant per callsite. Interning the callsite (keyed by `Metadata`
pointer identity -- already available on `CondensedEvent`) into a small per-segment table, and
storing one index per event, removes that redundancy.

**Approach**: New `callsite_table` module. `EventBuffer` interns each distinct callsite into a
`CallsiteTable` holding `(target_idx, file_idx, line, level)` and pushes the resulting index to a new
`col_callsite_indices` (RLE). Removed `col_levels`, `col_target_indices`, `col_file_indices`,
`col_lines`. The decoder reads the callsite table once, then resolves every field from the per-event
index.

**Result**: **213,298 events** (+4.3% over Trial S; **+40.6% over the pre-round 151,682 baseline**).
Real combined b/evt: 9.52 → 9.21.

**Why the synthetic gain understates production**: the benchmark builds 60 distinct callsites (15
targets × 4 levels) and picks target and level *independently* per event, so `callsite_indices`
carries the full target×level entropy (0.95 b/evt) and never forms runs. In production a callsite has
*one* level and emits events in bursts, so `callsite_indices` forms long RLE runs (→ near-zero) and
the four collapsed columns cost essentially nothing. The +4.3% here is a conservative floor; the
production win is materially larger. This is also the architecturally correct model -- a log event's
provenance *is* its callsite.

### Breakdown after Trial CS (final: msg-var split reverted, single `msg_variables` column)

| Column | compressed b/evt | % of total |
|--------|-----------------:|-----------:|
| **field_values** | 3.34 | **36.8%** |
| **msg_variables** | 2.24 | 24.6% |
| callsite_indices | 0.95 | 10.4% |
| timestamps | 0.73 | 8.0% |
| msg_template_indices | 0.71 | 7.8% |
| field_key_indices | 0.56 | 6.2% |
| field_counts | 0.40 | 4.4% |
| string_table | 0.14 | 1.5% |
| callsite_table | 0.03 | 0.3% |

(Real combined-frame total: **9.17 b/evt**; meta 15,090 + content 23,009 compressed over 4,153 events.)

`field_values` and `msg_variables` (the raw high-entropy text) are now ~61% of the footprint. For
the *synthetic* generator these are near their entropy floor (half the field values are uniform-random
6-digit numbers), so further column-structure wins are small; the remaining levers are workload-
dependent and covered under benchmark-fidelity below.

## Stability re-check (Trial I property preserved)

`ring_buffer_stability_bench` (1.5M events, default config) after Trials T1+V+CS (splits reverted):

| Metric (128k segments) | Trial I (pre-round) | This round | Change |
|------------------------|--------------------:|-----------:|-------:|
| Min retained           | 150,246             | 209,655    | +39.5% |
| Avg retained           | 152,368             | 212,366    | +39.4% |
| Min / avg ratio        | 98.6%               | 98.7%      | flat   |
| Max drop amplitude      | 3,411               | 4,126      | +21%   |
| Min coverage duration   | 63.8m               | 89.1m      | +40%   |

The min/avg ratio is unchanged, so Trial I's "no monster segments, uniform eviction" property is
fully preserved. Drop amplitude rose slightly in *absolute events* only because each 128 KiB segment
now packs ~30% more events (they are smaller); as a fraction of retained it is still ~2%. Net: the
fixed 2 MiB buffer now holds ~40% more logs and covers ~40% more wall-clock time, with identical
stability characteristics.

---

## Fresh-round summary (2026-07)

| Stage | Retained (500k cap bench) | vs pre-round |
|-------|--------------------------:|-------------:|
| Pre-round baseline (post-Trial-I: ms→ns timestamps, row of columns) | 151,682 | -- |
| + T1: millisecond timestamp precision | 198,268 | +30.7% |
| + V: drop redundant per-event `var_count` | 204,434 | +34.8% |
| + CS: callsite interning (target/file/line/level → table + index) | **213,298** | **+40.6%** |

**Applied and kept** (all composing with the pre-existing Trial I stability gate):
1. `TIMESTAMP_GRANULARITY_NS = 1_000_000` -- ms-quantized, drift-free delta timestamps (`codec.rs`).
   **This is lossy** (sub-ms precision discarded; ordering preserved) and is the single biggest win.
2. `var_count` no longer stored per event; derived from the template's `\0` count.
3. Callsite interning: new `callsite_table.rs`; `EventBuffer` holds a `CallsiteTable` +
   `col_callsite_indices`; four per-event columns removed.

**Tried and reverted** (kept out of the tree):
- Trial S (split message-variable length/bytes): neutral on this benchmark, reverted for
  consistency + production safety (see Trial FV lesson).
- Trial FV (split field-value length/bytes): **-1.8% regression** -- length↔value co-location matters.

**Verification state**: 32 ring-buffer unit tests pass; `cargo check --workspace --tests` clean;
`make fmt` applied. Stability bench: min retained 150,246 → 209,655, min/avg 98.7%.

### Open items / next levers (ranked, for the next session)

1. **Benchmark fidelity (do this FIRST -- it gates the value of everything below).** The generator
   (`benchmarks.rs::EventGenerator`) picks target/level/file/message i.i.d. per event -- *no temporal
   locality*. Real components log in bursts from a few callsites. Consequences:
   - RLE columns (`callsite_indices`, `msg_template_indices`, `field_counts`) show `(count=1,value)`
     overhead here instead of long runs, so their true production cost is far lower and callsite
     interning wins much more than the measured +4.3%.
   - Timestamp deltas here are uniform 1-50 ms; real bursts give sub-ms deltas that quantize to 0
     (RLE-friendly), so ms timestamps win even more in production.
   - Field values here are 50% uniform-random numbers (near-incompressible); real field values repeat
     more, so `field_values` compresses better in production.
   Action: add a `bursty`/locality mode to `EventGenerator` (pick a "current callsite" and emit a run
   of N events from it with small ts deltas and a stable level before switching) and re-run
   `ring_buffer_column_breakdown` + capacity. Then re-evaluate the levers below against realistic data.
   Do NOT optimize further against the i.i.d. generator -- risk of overfitting (Trial FV nearly was).
2. **`field_values` (37%) and `msg_variables` (25%)** are the remaining bulk. Options, all
   workload-dependent (validate on the bursty bench first):
   - Numeric field-value specialization (Approach G): parse all-digit values to varint. NOTE the
     earlier analysis in "Approach G" is now suspect -- zstd already codes digit strings near their
     entropy; a raw varint may be *less* compressible. Measure, don't assume.
   - Field-value interning was rejected long ago (Trial 5) but under realistic (repetitive) values it
     may now pay off -- worth re-testing on the bursty bench.
   - Zstd dictionary trained on representative segments (Approach B): would help the many small
     high-entropy frames share context; also the only path to sub-segment-granularity stability.
3. **`callsite_indices` (10%)**: in production this RLEs to near-zero; confirm on bursty bench, then
   leave alone. If still material, delta-or-move-to-front could help.
4. **Timestamp precision as config**: currently a module const. If any consumer needs finer than ms,
   promote to `RingBufferConfig` AND write the granularity into the segment header (decoder must
   scale by it). Left as a const for now (simplest; ms is the right default). **DECIDED 2026-07-14**:
   the human confirmed lossy millisecond precision is acceptable — keep it as the default const; do
   not promote to config unless a concrete consumer needs sub-ms. (See "Decisions confirmed" at the
   end of the Benchmark Fidelity Round.)
5. **`msg_template_indices` (8%)**: RLE of skeleton-table indices; random here, runs in production.
   Tied to the Drain clustering quality (Trial D). Revisit only after fidelity fix.

### How to reproduce the measurements
- Capacity: `cargo test --release -p saluki-app --lib ring_buffer_capacity_bench -- --ignored --nocapture`
- Per-column map: `... ring_buffer_column_breakdown ...` (the ground-truth prioritization tool;
  backed by `EventBuffer::column_breakdown()`, test-only).
- Stability (min retention, the metric that actually matters): `... ring_buffer_stability_bench ...`
- Sweeps: `ring_buffer_capacity_sweep`, `ring_buffer_stability_sweep`.
- Retained count is quantized by whole-segment eviction (±~4k ≈ ±2%); use the breakdown's real
  combined `bytes/event` as the sensitive metric for sub-2% changes.

### New tools added this round (2026-07, benchmark-fidelity)
- **`GenMode::Bursty`** in `benchmarks.rs`: the realistic locality generator. Every benchmark now has
  a bursty variant — `ring_buffer_column_breakdown_bursty`, `ring_buffer_capacity_bench_bursty`,
  `ring_buffer_stability_bench_bursty`. **Evaluate all future content-column levers against the bursty
  variant, never the i.i.d. one.**
- **`ring_buffer_dictionary_ceiling`**: overfit-proof upper bound on any cross-segment / zstd-dictionary
  scheme (independent vs concatenated frame compression), for both modes.
- The i.i.d. tests are unchanged and still reproduce every historical number exactly.

---

# Benchmark Fidelity Round (2026-07)

The open-items list flagged this as the gating task: the `EventGenerator` picked target/level/file/
message **i.i.d. per event**, so it had zero temporal locality. Real components log in *bursts* from a
small set of callsites, each with a fixed level and message template. Without modeling that, the
benchmark structurally understated every locality-dependent win (RLE columns, ms-timestamp
quantization, verbatim field-value repetition) and risked overfitting content-column experiments to
synthetic random data (Trial FV nearly was overfit this way).

## Trial BF: add a `bursty`/locality mode to `EventGenerator`

**Approach**: Added a `GenMode { Iid, Bursty }` to `EventGenerator` (`benchmarks.rs`). The **Iid path
is byte-for-byte unchanged** — it consumes no RNG during construction, so every previously recorded
i.i.d. number still reproduces exactly (verified: column_breakdown still 9.17 b/evt, capacity still
213,298). Bursty mode:

- Draws a fixed table of `BENCH_BURSTY_CALLSITES = 48` callsites up front. Each callsite has a stable
  `&'static Metadata` (leaked, so pointer-identity interning works exactly as in production), a fixed
  level (weighted 40/30/20/10 like Iid), a fixed message template (60% a static short message, 40% a
  formatted archetype — any embedded endpoint is **fixed per callsite**, so Drain keeps it as static
  text), and a fixed field shape (same key set every event; each field value is either a fixed string
  stable per callsite, or a per-event random number).
- Emits events in **contiguous bursts**: pick a callsite, emit a run of 4–39 events from it, then
  switch. Within a burst, timestamps advance by only 5–300 µs (sub-ms → deltas quantize to mostly 0);
  between bursts, a 1–40 ms gap.

New ignored tests: `ring_buffer_column_breakdown_bursty`, `ring_buffer_capacity_bench_bursty`,
`ring_buffer_stability_bench_bursty`. The Iid tests and sweeps are untouched.

**Design note — bursts are contiguous.** Real production traffic interleaves bursts from concurrent
tasks, so reality sits *between* fully-i.i.d. (pessimistic: no runs) and fully-contiguous bursts
(optimistic: maximal runs). Bursty mode is the optimistic bound; i.i.d. is the pessimistic one. The
truth is bracketed by the two, and both are now measurable.

### Result: the two content columns are ~87% of realistic footprint

Column breakdown, single ~128 KiB segment, zstd=19:

| Column | i.i.d. b/evt(c) | i.i.d. % | **bursty b/evt(c)** | **bursty %** |
|--------|----------------:|---------:|--------------------:|-------------:|
| **field_values** | 3.34 | 36.8% | **2.78** | **58.3%** |
| **msg_variables** | 2.24 | 24.6% | **1.36** | **28.5%** |
| timestamps | 0.73 | 8.0% | 0.17 | 3.6% |
| string_table | 0.14 | 1.5% | 0.15 | 3.1% |
| field_key_indices | 0.56 | 6.2% | 0.08 | 1.7% |
| callsite_indices | 0.95 | 10.4% | 0.07 | 1.5% |
| msg_template_indices | 0.71 | 7.8% | 0.07 | 1.4% |
| field_counts | 0.40 | 4.4% | 0.06 | 1.2% |
| callsite_table | 0.03 | 0.3% | 0.03 | 0.6% |
| **combined total** | **9.17** | | **4.75** | |

Every prediction in the open-items list is confirmed:

- **All metadata/RLE columns collapse under locality.** `callsite_indices` 0.95 → 0.07 (−93%),
  `msg_template_indices` 0.71 → 0.07, `field_counts` 0.40 → 0.06, `field_key_indices` 0.56 → 0.08:
  each now forms long runs / repeats verbatim. `timestamps` 0.73 → 0.17 (sub-ms deltas quantize to 0).
  Callsite interning (Trial CS) and ms timestamps (Trial T1) win *far* more here than the i.i.d.
  numbers showed — exactly as predicted.
- **`field_values` + `msg_variables` are now 86.8% of the compressed segment** (up from 61% i.i.d.).
  Everything else combined is ~13%. **These two columns are the entire optimization game under
  realistic data.** `field_values` alone is 58.3%.
- `field_values` still doesn't fully collapse (2.78 vs 3.34) because half its values are per-event
  random numbers by construction; the fixed-string half does dedup well. Real workloads likely repeat
  more, so this is still a conservative floor.

### Capacity and stability under bursty data

| Metric | i.i.d. | bursty |
|--------|-------:|-------:|
| Capacity retained (500k fed, 2 MiB) | 213,298 (42.7%) | **410,879 (82.2%)** |
| Overall compression ratio | 4.01x | 6.18x |
| Avg compressed bytes/event | 9.3 | 4.8 |
| Stability min retained (1.5M fed) | 209,809 | **399,550** |
| Stability min/avg ratio | 98.7% | 98.0% |
| Stability max drop amplitude | 4,126 | 5,387 |

The fixed 2 MiB buffer holds **~93% more events** under realistic locality than the i.i.d. generator
suggested. Trial I's "no monster segments, uniform eviction" stability property is fully preserved
(min/avg 98.0%). Drop amplitude is slightly higher in absolute events only because each 128 KiB
segment now packs ~2x more events (they compress to ~4.8 B/evt); as a fraction of retained it is
~1.3%.

> **Coverage-duration caveat**: bursty mode's synthetic event *rate* is much higher than i.i.d.
> (bursts of ~21 events over a few ms, then a ~20 ms gap ≈ ~1 ms/event, vs i.i.d.'s ~25 ms/event), so
> the reported coverage minutes are not comparable across modes. Only the retention **count**
> distribution is a like-for-like comparison; coverage is a function of the (arbitrary) synthetic rate.

### Consequences for the remaining levers (re-ranked against bursty data)

The i.i.d. ranking put `field_values` at 37% and `msg_variables` at 25%. The realistic ranking makes
them **58% and 29%** — everything else is noise. Therefore:

- **`field_values` (58%) is now the overwhelming target**, not merely the largest of several. Numeric
  field-value specialization and field-value interning both act here and are worth the most.
- **`msg_variables` (29%) is second.** Same techniques (numeric specialization, interning of repeated
  verbatim tokens) apply.
- **All the RLE/index columns are dead as optimization targets** under realistic data — they already
  cost ~0.06–0.08 b/evt. Do not spend effort there; the i.i.d. numbers that made them look like 6–10%
  targets were an artifact of the broken generator.
- A **zstd dictionary** (Approach B) looked attractive here (with locality the metadata frame is tiny
  and the content frame carries almost everything, so cross-segment shared context seemed promising) —
  but Trial DC below *measured* the ceiling at ≤4% (content-frame ceiling only 1.4%) and rules it out.

**Verification**: all 32 ring-buffer unit tests pass; `cargo check --release -p saluki-app --lib
--tests` clean. Iid reproducibility re-confirmed (9.17 b/evt, 213,298 retained). No production code
changed — this trial only touches `benchmarks.rs` (test-only).

## Trial NG: numeric field-value specialization (REJECTED — regression on both modes)

**Hypothesis (Approach G, re-tested against bursty data)**: `field_values` is 58% of the realistic
footprint; half its values are integers. Parsing canonical `u64` values and storing them as a varint
(tagged `varint(0) varint(n)`, everything else `varint(len+1) bytes`, co-located to preserve zstd
matching) shrinks e.g. a 6-digit `"948576"` from 7 bytes (length + digits) to a 3-byte varint
uncompressed.

**Approach**: strict round-trip-safe `parse_canonical_u64` (rejects leading zeros, non-digits,
>u64); `write_value`/tagged decode in the field-value stream. Full unit-test coverage of the
round-trip (37 tests passed).

**Result**: **regression on both modes.** Uncompressed `field_values` shrank as expected, but
*compressed* size grew:

| Metric | before | after NG |
|--------|-------:|---------:|
| i.i.d. capacity retained | 213,298 | 210,714 (**−1.2%**) |
| bursty capacity retained | 410,879 | 403,115 (**−1.9%**) |
| i.i.d. breakdown total b/evt | 9.17 | 9.33 |
| bursty breakdown total b/evt | 4.75 | 4.84 |

**Why it hurt (confirms the open-item's suspicion — "measure, don't assume")**: the benchmark's
integers are uniform-random in `[0, 1e6)` (~19.9 bits ≈ 2.5 bytes of information; a 6-digit string is
6·log2(10) = 19.9 bits by construction). zstd's FSE literal coder already crushes digit strings —
which use only 10 of 256 byte symbols, ~3.32 bits each, a *concentrated* distribution — to near that
2.5-byte floor. The varint bytes are *not* incompressible (FSE still entropy-codes them), but a varint
smears three different byte-position distributions (two continuation bytes near-uniform over
`[128,255]`, one final byte over a smaller range) into a single FSE context, coding at ≈2.8 bytes
rather than 2.5 — plus a tag byte. So the loss comes from FSE's lack of per-position context on the
mixed varint stream, not from the bytes being random. Varint specialization can only win when either
(a) the integers are small enough that the varint is shorter than the entropy-coded digits, or (b)
they repeat *at long distances* where an explicit code would beat zstd's LZ (see Trial IV — but that
is the interning lever, not varint width). Uniform-random 6-digit numbers hit neither. **Reverted;
not in the tree.**

**Note for a future numeric-heavy workload**: if a real deployment's numeric field values are
dominated by small (`< ~2^14`) or highly-repeated integers, revisit — but gate on a
production-representative benchmark, never the uniform-random synthetic values.

## Trial IV: selective field-value interning (REJECTED — overfit; large regression on high-cardinality)

**Hypothesis (re-test of Trial 5 under realistic data, per open-item #2)**: `field_values` is 58% of
the bursty footprint. Interning *non-numeric* values (which repeat verbatim in real logs) into the
shared string table — storing `varint(0) varint(index)` instead of the bytes — while keeping
canonical integers inline (they are unique per event; interning them was Trial 5's failure mode)
should beat zstd's LZ matching for the repeated strings.

**Approach**: `is_canonical_integer` gate; non-integer values interned into `StringTable`, integers
inlined; tagged decode in the field-value stream (tag 0 = interned index, else `len+1` = verbatim).

**Result on the standard bench**: a large apparent win — bursty capacity **410,879 → 441,596
(+7.5%)**, i.i.d. **213,298 → 229,187 (+7.4%)**, and the breakdown's total b/evt fell on both modes.
Interning is inherently locality-independent (it deduplicates globally over the whole-segment table),
so gaining in i.i.d. as well is *expected*, not itself proof of anything — it only rules out locality
as the source and points at value cardinality. (If anything, i.i.d.'s scattered repeats mean larger
LZ offsets, so zstd's own matching is weaker there and interning "should" win slightly *more* in
i.i.d.; that it roughly ties bursty is a minor curiosity, not load-bearing.) The decisive question —
is the win an artifact of the value *cardinality*? — needs the experiment below.

**Cardinality-sensitivity experiment (the decisive test)**: the generator draws non-numeric field
values from a pool of **8** fixed strings, so each appears hundreds of times per segment — interning's
best case. I added a temporary toggle making string field values **unique per event** (a request-ID /
UUID-like worst case) and ran the 2×2 (bursty capacity retained):

| string cardinality | intern OFF | intern ON | Δ |
|---------------------|-----------:|----------:|------:|
| low (8-value pool)  | 410,879    | 441,596   | **+7.5%** |
| high (unique/event) | 249,613    | 205,674   | **−17.6%** |

Interning is a **coin-flip on the real workload**: it helps when string field values are
low-cardinality and *badly hurts* (−17.6%) when they are high-cardinality. Interning a value that
appears once costs a table entry (bytes stored anyway) **plus** a multi-byte index **plus** cross-frame
splitting — strictly worse than inlining, and it bloats the (currently uncounted) string table. Real
debug logs mix both kinds (component/status/level-ish fields repeat; request IDs, unique paths, and
specific error strings do not), so the sign of the effect is workload-dependent and unknowable from
the synthetic benchmark.

**Verdict: REJECTED and reverted** *for the unguarded form*, because its sign on a real workload is
unknowable from this benchmark: the +7.5% is dominated by the unrealistically small (8-value)
synthetic string pool, and unguarded it regresses −17.6% on high-cardinality fields (Trial 5 rejected
interning for the same underlying reason). Two additional problems surfaced even in the best case:
(1) `EventBuffer::size_bytes()` does not count the string table, so interned bytes move into an
*uncounted* accumulator — the encoder then over-fills each segment by event count (the flush gate
fires late), which is why the stability run showed segments holding far more events and drop amplitude
rising to 9,665 (bursty min/avg 98.0% → 96.7%). That drop-amplitude jump is an artifact of the
accounting gap, **not** a consequence of the +7.5% compression gain (a genuine +7.5% would raise
events/segment ~7.5%, not ~2×); it is a symptom of problem (1), and would have to be fixed first.
(2) For high-cardinality values, interning is *strictly worse* than inlining (table entry + index +
cross-frame split, for a value that never repeats).

**Not fully dead — a guarded version is the defensible follow-up, but only against real logs.** A
cardinality-guarded interner (intern a value only once it has actually repeated, or cap the interned
table and inline the overflow) converts the coin-flip into win-or-neutral and directly targets the
58%-of-footprint column. It is *not* pursued here for one reason: its benefit and the guard's
threshold cannot be honestly calibrated on an 8-value synthetic pool — doing so would just re-encode
the generator's structure into the heuristic (the exact overfitting this round exists to prevent).
The prerequisites for a real attempt are: the `size_bytes()` accounting fix, the frequency/size
guard, and a **production-representative log corpus** to tune and validate against.

## Lever re-evaluation summary (against bursty data)

All three content-column levers from the open-items list were tested against the realistic (bursty)
benchmark and **rejected**; the current encoding is at/near the achievable frontier for this workload
without workload-specific training or lossy transforms:

| Lever | Result | Disposition |
|-------|--------|-------------|
| Numeric field-value specialization (Trial NG) | −1.2%/−1.9% | rejected — zstd's entropy coder already codes uniform-random digit strings near their information floor; a fixed varint is *less* compressible |
| Selective field-value interning (Trial IV) | +7.5% low-card / **−17.6% high-card** | rejected (unguarded) — overfit to the 8-value pool; a guarded version needs real logs |
| zstd dictionary (Approach B, Trial DC) | **≤4% ceiling** (content frame only 1.4%) | not worth the complexity; see below |

**On the zstd dictionary (Approach B) — now measured, not assumed (Trial DC).** Earlier notes called
this "the one remaining lever" on the assumption that per-segment re-learning of the common content
vocabulary is costly. That assumption was never measured — so `ring_buffer_dictionary_ceiling`
measures an **overfit-proof upper bound** with zero training: compress N=16 segments' frames
independently (current behavior) versus concatenated into one frame. Concatenation lets each segment
reference *all* prior segments — strictly more shared context than any fixed-size dictionary — so the
saving is a hard ceiling on any dictionary scheme, and it involves no memorization of the generator's
vocabulary (which is why a *trained* dictionary was correctly never prototyped: it would post a fake
win).

Result: the ceiling is **~3.0% (i.i.d.) / ~4.2% (bursty)** of total compressed bytes. Crucially, under
bursty the **content frame** — which is 87% of the footprint — has a ceiling of only **1.4%**; the
`content` is already at its cross-segment entropy floor (repeated verbatim values are matched
*within* a segment; there is little cross-segment structure left). The bursty meta frame shows a 22%
ceiling, but the meta frame is only ~13% of the total (and already just ~2.7 KB compressed/segment),
so it contributes ≤3% overall.

**Conclusion**: a zstd dictionary is **not worth its complexity** (training corpus, embed-vs-config
storage decision, retraining as log shapes drift) for a ≤4% ceiling that lives mostly in the
already-tiny meta frame. This also closes out the "sub-segment-granularity stability via shared
context" idea from Trial I — there isn't enough shared context to exploit.

**Net outcome of this round**: the benchmark-fidelity fix (Trial BF, bursty generator) is the shipped
deliverable. It corrected the entire prioritization (content columns are ~87% of realistic footprint,
not 61%) and then let us *disprove*, with data, that any of the content-column levers pay off on a
realistic workload: numeric specialization regresses, unguarded interning is overfit/coin-flip, and
the dictionary ceiling is ≤4%. **The current encoding is at/near the achievable frontier for this
workload; no production encoding change is recommended.** The only remaining paths both require
production log data: a cardinality-guarded field-value interner (Trial IV follow-up) and/or revisiting
numeric specialization *if* real numeric values turn out to be small/repetitive.

### Decisions confirmed (human, 2026-07-14)

1. **Lossy millisecond timestamp precision is accepted** as the default (`TIMESTAMP_GRANULARITY_NS =
   1_000_000`). ms is the standard resolution for human-facing log output; sub-ms event ordering is
   preserved by storage order regardless. Not promoted to `RingBufferConfig`; revisit only if a
   concrete consumer needs sub-ms (then also write the granularity into the segment header).
2. **This round ships as the benchmark-fidelity fix only** (bursty generator + dictionary-ceiling
   tool + these notes); no production encoding change. The next real gain requires a
   production-representative log corpus, at which point the guarded-interning follow-up is the
   first thing to evaluate.

---

# Real-data harvest round (2026-07): the bottleneck was a single DEBUG callsite

The open item "next gain requires production log data" was acted on by building a **capture -> replay
harness** and harvesting a real corpus from `agent-data-plane` under a DogStatsD workload. The result
overturned the synthetic conclusions entirely: on real traffic the dominant cost was not an encoding
detail at all, but **one verbose log statement**.

## The harness (harvest half)

> **The harness tooling described here was one-time scaffolding and was removed from the tree once
> the optimization concluded** (it was not part of the shipping feature, and its capture tap was
> dormant code in the production binary). It is recorded here as the *method* that produced the
> finding below; the recipe in "Reproduce" is enough to rebuild it if a future harvest is needed.

The harvest half was:

- **A development-only capture tap** (`capture.rs`, gated by a `SALUKI_RING_BUFFER_CAPTURE_PATH` env
  var, off / zero-overhead otherwise) that teed every structured `CondensedEvent` the processor saw
  to a JSON-Lines file. It wrote the *structured* event (callsite + message + separated key/value
  fields), not a rendered line, so replay was faithful.
- **A container harness** (`test/ring-buffer-harvest/harvest.sh`) that ran the converged Datadog
  Agent + ADP image (`make build-datadog-agent-image-release`) under `lading`
  (`ghcr.io/datadog/lading:0.31.2`) driving a fixed-rate DSD workload (SMP
  `quality_gates_rss_dsd_medium` shape, ~10k contexts, 10 MiB/s) for a short fixed period, then
  stopped gracefully and dropped out the corpus.
- **A replay half** (`ring_buffer_replay_*` benches) that reconstructed `CondensedEvent`s from a
  corpus -- interning one `&'static Metadata` per distinct `(target, level, file, line)` -- and ran
  the same measurement tooling as the synthetic benches, with a round-trip test proving the
  capture->corpus->replay reconstruction was byte-faithful.

## Finding: 87% of the DEBUG stream was one callsite dumping a huge field

A 40 s DSD harvest produced **11,845 events / 28 MB**. Breakdown of where it went:

- **10,336 of 11,845 events (87%)** came from `saluki_context::resolver` logging
  `"Resolved new context."` at DEBUG, each with `?context` -- a full `Debug`-dump of the `Context`
  struct (name + every tag), ~2.4 KB mean, up to 5.3 KB.
- Steady-state single-segment breakdown: **`field_values` = 99.8%** of the footprint, ~2,298
  uncompressed / **1,432 compressed** bytes/event, compressing only **1.6x** (each context is unique
  high-entropy text). Everything else (timestamps, callsite, message, keys) was rounding error.
- Capacity: only **2,117 / 11,845 retained (17.9%)** in the 2 MiB buffer; 1.59x overall.

This is ~100x larger `field_values` than the synthetic generator modeled (21 uncompressed b/evt),
and it is a workload the synthetic bench structurally *could not* surface. It also confirms, on real
data, that the rejected encoding levers were the right calls here: interning a unique 2.4 KB context
dump is strictly worse (Trial IV high-cardinality case), and it is not numeric.

## Fix: move the context-resolution logs to TRACE

The ring buffer captures DEBUG-and-above (`LevelFilter::DEBUG`), so the single highest-leverage change
was **logging-side, not encoding-side**: `lib/saluki-context/src/resolver.rs` lines 541 + 570 changed
from `debug!` to `trace!` (`"Resolved new non-cached context."` and `"Resolved new context."`; both
dump the full `?context`). TRACE is below the ring buffer's filter, so these drop out of the buffer
entirely. (Human-approved 2026-07-14.)

### Effect (re-harvested with the change, same workload)

| metric | before (DEBUG) | after (TRACE) |
|--------|---------------:|--------------:|
| events captured in 40 s | 11,845 | **1,693** (−86%) |
| corpus size | 28 MB | **444 KB** (−98%) |
| steady-state compressed b/evt | 1,432 | **3.17** (~450x smaller) |
| content-frame compression | 1.6x | **20.7x** |
| `field_values` share | 99.8% | 26% (no column dominates) |
| 40 s workload vs 2 MiB buffer | overflowed to 17.9% | fits whole, <1 segment used |

With the giant field gone, the residue is exactly what the columnar encoding is built for: a diverse
mix of small, highly repetitive operational logs (`"Found expired items."`, `"Forwarding events."`,
transaction-queue IO, ...), compressing to **3.17 b/evt** -- *better* than the synthetic bursty bench
(4.75), because real operational logs repeat more. The buffer's effective time-coverage for this
workload improved by roughly 1-2 orders of magnitude.

## Takeaways

1. **The single biggest real-world lever for ring-buffer coverage was a logging-volume decision at
   one callsite**, invisible to every encoding trial. The harvest->replay harness is what surfaced it;
   the synthetic benchmark could not have.
2. **Fidelity-first paid off twice**: the bursty generator corrected the *relative* column ranking,
   and the real corpus corrected the *absolute* magnitudes (and found the outlier callsite).
3. **Caveat**: this is one workload (DSD, high context-churn) in a churn-heavy phase; other
   workloads/phases differ. The `trace!` change also removes these lines from DEBUG-level *console*
   output -- anyone who relied on them must now use TRACE. Worth harvesting OTLP / steady-context DSD
   profiles before further encoding work; with the context dump gone, no single column dominates, so
   remaining encoding gains look incremental.

### Rebuilding the harness (if a future harvest is ever needed)
The tooling was removed after use; to reconstruct it:
1. Re-add a dev-gated capture tap on the processor thread that tees each `CondensedEvent` (its
   structured fields, not a rendered line) to a JSON-Lines file when an env var is set.
2. Build the converged image (`make build-datadog-agent-image-release`) and run it with the capture
   env var set and an output dir mounted; drive DSD with `lading` (`ghcr.io/datadog/lading:0.31.2`,
   entrypoint `lading`) using a `dogstatsd` `unix_datagram` generator over a shared socket volume
   (`--no-target`, `--warmup-duration-seconds 0 --experiment-duration-seconds N`, a `--prometheus-addr`
   to satisfy its mandatory telemetry; `bytes_per_second` is a sibling of `variant`, not inside the
   payload). Mirror the SMP `quality_gates_rss_dsd_medium` payload shape. Stop the container
   gracefully to flush the capture.
3. Replay offline: reconstruct `CondensedEvent`s (one leaked `&'static Metadata` per distinct
   `(target, level, file, line)`), feed `ProcessorState`, and reuse `EventBuffer::column_breakdown()`.

## Optimization hunt against the real corpus (post-trace-fix)

Harvested a larger, steady-state corpus (300 s DSD, **12,800 events / 3.3 MB**) and re-ran the hunt
against it. Real steady-state (128 KiB segment, 5,616 events): **1.59 compressed b/evt**, content frame
**28x**. Column ranking (very different from synthetic): `field_values` 41% (0.65 b/evt), `timestamps`
16% (0.26), `string_table` 10%, `msg_template_indices`/`callsite_indices` ~9% each. Everything is tiny;
the trace fix was the real win.

**Two model-correcting findings from the real data:**
- **Real logging is *interleaved*, not bursty**: consecutive-event callsite/template run length averages
  **1.1** (65 distinct callsites, ~113 distinct messages, all concurrently active). The bursty generator
  (mean run ~21) therefore *over-states* the RLE/locality wins; for the index columns reality is near the
  i.i.d. end of the bracket, not the bursty end.
- **Out-of-order timestamps occur** (min inter-event delta **−1 ms**; concurrent writer threads stamp
  before the MPSC channel). The encoder's `saturating_sub` delta then stores 0 and the decoder drifts
  upward for the rest of the segment. Magnitude is ~ms and bounded per segment, so it is a minor fidelity
  nit, not a serious bug — but the (always-monotonic) synthetic generator never exercised it. A signed
  (zig-zag) delta would fix it if ever deemed worth it.

**Levers measured against the real corpus** (zstd-19, isolation, same 5,616-event window):

| Lever | current | candidate | verdict |
|-------|--------:|----------:|---------|
| Numeric field-value specialization | 0.654 | 0.683 b/evt (**+4.4%**) | **Reject** — same as synthetic (Trial NG); zstd codes the high-card numerics (`compressed_len`, `uncompressed_len`, …) near their entropy floor. |
| RLE → raw for index columns | 0.147 | 0.125 b/evt (**−15% of col, ~−2.8% total**) | **Not worth it** — a real but tiny win from the interleaving; RLE is more robust (wins big if a workload *is* bursty). |
| **Selective field-value interning** (non-numeric → table, numeric inline) | 0.654 | 0.571 b/evt (**−12.7% of col, ~−5% total**) | **Real win, candidate to pursue.** |

**The interning result reframes Trial IV.** On synthetic I rejected it as "overfit to the 8-value
pool." But the real DSD corpus has only **9 distinct non-numeric field values** (addresses, endpoints,
encoder names, statuses) repeated thousands of times — real ADP operational logs genuinely have
low-cardinality string fields. So the synthetic +7.5% was *predictive for this workload class*, not
overfit. The −17.6% high-cardinality case (Trial IV) is still the risk for workloads with unique string
fields (request IDs, unique paths) — ADP's operational logs seem not to have those, but other workloads
might. So the shippable form is the **guarded** selective interner (intern non-numeric values, but cap
the intern table / only intern values seen ≥ 2× so an unexpected high-card workload degrades to inline),
plus the `EventBuffer::size_bytes()` accounting fix (interned bytes move to the currently-uncounted
string table).

**Recommendation.** Post-trace-fix the buffer already covers a very long window (1.59 b/evt, hours of
coverage on this workload), so every remaining encoding lever is marginal in practice. The single
worthwhile candidate is **guarded selective field-value interning (~5%)**; numeric specialization is
confirmed-rejected on real data, and RLE→raw is too small/non-robust. The out-of-order-timestamp drift
is a separate minor correctness nit. None is urgent; all are ~single-digit-% of an already-tiny footprint.

---

# Capstone: where we started, where we went, and why

**Decision: stop here.** The compressed ring buffer's encoding is near-optimal on real ADP traffic;
no further encoding change is worth its complexity.

## Where we started
A row-oriented `musli` + zstd ring buffer retaining **68,708** events (500k-event synthetic bench).
Successive *encoding* rounds — hand-rolled columnar encoding, per-segment string + callsite interning,
RLE, split meta/content frames, Drain-style message templating, millisecond timestamps, and the
stability flush-gate — roughly **tripled** that to **213,298 (+210%)**. Every number was measured
against a generator that picked target/level/file/message **i.i.d.**, with no temporal locality.

## Where we went (this arc)
1. **Made the benchmark honest before optimizing further.** Added a *bursty* generator modeling how
   components actually log (runs from one callsite). It changed no encoding — it changed the *picture*:
   the two high-entropy content columns are ~87% of a realistic segment; the RLE/index columns collapse.
   This corrected the whole prioritization and prevented over-fitting to i.i.d. artifacts.
2. **Re-evaluated every content-column lever against realistic data — and rejected them all.** Numeric
   specialization regressed (zstd already codes digit strings near their entropy floor); unguarded
   interning was workload-dependent (+7.5% low-card / −17.6% high-card); the zstd-dictionary ceiling was
   an overfit-proof **≤4%**. On the synthetic bench, the encoding was already at its frontier.
3. **Got real data.** Built a (throwaway) capture→replay harness — a dev-gated event tee, a
   converged-image + `lading` DSD harvest, and deterministic offline replay — exactly what the notes
   said the remaining levers needed. (Removed after use; see "The harness" above.)
4. **Real data found the actual bottleneck — and it was not the encoding.** 87% of the real DEBUG
   stream (98% of bytes) was one callsite, `saluki_context::resolver` "Resolved new context.", dumping
   the full `Context` struct as a field value. Moving it to `trace!` cut steady-state cost ~450× on a
   matched sample and lifted coverage ~1–2 orders of magnitude. No encoding trial could have found it.
5. **Re-ran the hunt on real data.** Post-fix the encoding is near-optimal (**1.59 b/evt**, content
   frame 28×). Numeric-spec stayed rejected; the one real lever (guarded field-value interning) is ~5%
   and marginal. Also learned real logging is **interleaved, not bursty** (the bursty bench over-states
   RLE wins) and carries **out-of-order timestamps** (a minor delta-drift nit).

## Why
One principle throughout: **optimize against data that looks like production, or you optimize the wrong
thing.** The i.i.d. generator hid locality; the synthetic field values understated real sizes ~100×;
neither contained the one verbose callsite that dominated reality. The biggest wins of the whole effort
were the early encoding rounds (≈3×) and then — once the measurement finally matched reality — a
*one-line log-level change*. Investing in benchmark fidelity, and then in real data, paid off more at
the end than any further encoding cleverness could have.

## Where we ended
- **Kept:** the `debug!`→`trace!` fix (the real win), the synthetic bursty benchmark suite (the
  `GenMode::Bursty` generator + `ring_buffer_column_breakdown` / `dictionary_ceiling` diagnostics, for
  future perf work), and these notes.
- **Removed after use:** the real-data capture→harvest→replay harness (dev-gated capture tap, the
  `test/ring-buffer-harvest/` container harness, and the replay benches). It was one-time scaffolding,
  not part of the shipping feature; the method + rebuild recipe are preserved above.
- **Deliberately not shipped:** numeric specialization, field-value interning (net-negative or
  marginal/workload-risky), a trained zstd dictionary (≤4% ceiling, overfit risk).
- **Open, low-priority (only if the memory budget tightens):** a *guarded* selective interner (~5%),
  and a zig-zag timestamp delta to remove the out-of-order drift. Evaluate both against a fresh real
  corpus, never the synthetic generator.
