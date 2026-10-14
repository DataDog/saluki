# Checks FIT wire contract, version 1

The semantic source is the existing `checks/v1/*.proto` definitions. This contract defines a separate binary encoding for shared memory. Changing any type ID, field order, field representation, or accepted encoding requires incrementing the application protocol version in both peers.

The application identity is the eight ASCII bytes `DDCHECKS`; the application protocol version is `1`. The generic FIT setup additionally checks its setup and ring-layout versions. The record type IDs are metric `1`, log `2`, service check `3`, and event `4`. A record has exactly one logical `CheckData` alternative. The enclosing FIT ring stores its own length/type header and eight-byte alignment padding; those bytes are outside the payload below.

All numeric fields are little-endian. An `i32` represents an enum's existing numeric value; unknown values are carried unchanged to the receiving application's semantic converter. A `u64` uses eight bytes. An `f64` uses its exact IEEE-754 bit pattern as a little-endian `u64`. A `string` is a little-endian `u32` byte length followed by that many UTF-8 bytes. A repeated string is a little-endian `u32` element count followed by each encoded string. Strings preserve empty values, and lists preserve order and duplicates. Every field is encoded, including zero/empty fields; there are no protobuf field tags or optional-field presence bits.

Fields appear in the same order as their `.proto` field numbers:

| Record | Payload fields in order |
| --- | --- |
| Metric, type 1 | `i32 metric_type`, `string name`, `f64 value`, `u64 timestamp`, `string[] tags`, `string hostname`, `u64 interval_secs` |
| Log, type 2 | `string message`, `i32 level` |
| Service check, type 3 | `i32 status`, `string name`, `string message`, `string[] tags`, `string hostname` |
| Event, type 4 | `string title`, `string text`, `i32 priority`, `string hostname`, `string[] tags`, `i32 alert_type`, `string aggregation_key`, `string source_type_name`, `u64 timestamp` |

The encoder checks all lengths and the absolute maximum payload before allocating. The decoder rejects truncation, invalid UTF-8, impossible lengths and counts, oversized payloads, and trailing bytes. It does not discard valid but unknown enum numbers; the application converter retains the existing behavior for those values. The actual ring may reject a message below the absolute maximum because its configured capacity or current contiguous free region is smaller.

The producer encodes an explicit batch in order, stages no more than the ring's usable capacity, and publishes the encoded prefix through one core batch. It stops at the first encoding failure or provably unfittable next record. The returned accepted count refers to published records. If the ring rejects before an encoding failure, the ring rejection wins. Notification failure is reported separately and does not roll back already published records. The consumer gets owned decoded data after core has released its ring bytes.
