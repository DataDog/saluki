# OTLP Trace Ingestion Optimization Ideas

## 1. Eliminate repeated linear attribute scans (largest CPU win)

`use_both_maps` is called very heavily (42 call sites) and each call scans attribute arrays again.

References: `transform.rs` (line 1003), `util.rs` (line 102), `mod.rs` (line 268).

Optimization: one-pass per-span extraction for hot keys (or cached index) and reuse across `get_otel_*` helpers.

## 2. Avoid building full JSON trees for span events/links unless required (CPU + memory)

Current path allocates `JsonMap`/`JsonValue` trees and then serializes to string for every span with events/links.

References: `transform.rs` (line 681), `transform.rs` (line 708), `transform.rs` (line 811), `transform.rs` (line 867).

Optimization: stream JSON directly into a `String` buffer (or gate this behind config for high-throughput mode).

## 3. Reduce resource tag construction overhead in translator (memory + CPU)

`resource_attributes_to_tagset` builds formatted strings for each attribute and `TagSet::insert_tag` does O(n) duplicate checking, creating O(n²) behavior in worst case.

References: `translator.rs` (line 31), `owned.rs` (line 31).

Optimization: use a fast-path when OTLP keys are unique, avoid duplicate checks per insert, and avoid `format!` in hot loop where possible.

## 4. Stop cloning trace-id metadata per span (small/medium, low risk)

`trace_id_hex` is cloned for each span before conversion.

References: `translator.rs` (line 81), `transform.rs` (line 96).

Optimization: pass `Option<&MetaString>` into `otel_span_to_dd_span` and clone only at insertion point if needed.

## 5. Remove intermediate `Vec<Event>` creation in translation path (memory spike reduction)

Translation currently returns a `Vec<Event>`, then caller re-iterates to push into buffers.

References: `translator.rs` (line 59), `mod.rs` (line 130), `mod.rs` (line 455).

Optimization: translate directly into `EventBufferManager` (or callback/iterator API) to avoid extra allocation and copying.

## 6. Cache resource tag lookups in encoder per trace (CPU)

Encoder does repeated `get_single_tag` scans and repeatedly creates owned `Source` strings.

References: `mod.rs` (line 452), `mod.rs` (line 777), `shared.rs` (line 49), `util.rs` (line 214).

Optimization: one-pass extract of required tag values and borrowed source metadata for the trace.

## 7. Sampler-side churn: repeated formatting + `shrink_to_fit` on retain path (CPU + alloc churn)

Sampling metadata formats rates repeatedly; `retain_spans` shrinks capacity each time.

References: `mod.rs` (line 248), `mod.rs` (line 409), `mod.rs` (line 121).

Optimization: precompute sampling-rate strings and remove/relax `shrink_to_fit` in hot paths.
