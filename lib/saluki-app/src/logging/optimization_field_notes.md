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
