---
slug: malformed-event-sc-no-crash
title: Malformed DSD event / service-check payloads never crash process or socket
type: Safety
priority: High
status: net-new (no SDK assertion exists)
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
---

# malformed-event-sc-no-crash

## Origin
Coverage gap: the existing catalog's untrusted-input property `malformed-dsd-no-crash`
(`property-catalog.md` Category E) is scoped to the **metric** codec + framing. It does NOT
exercise the two separate, always-on event / service-check codecs that parse untrusted bytes
on every DSD listener whenever DogStatsD is enabled (`run.rs:681-684`). These codecs are
~394 LOC (`event.rs`) and ~312 LOC (`service_check.rs`) of net-new nom parsing with their own
length-prefix, UTF-8, timestamp, and extension-chunk logic that the metric path never touches.

## Code paths (file:line)
- Event codec entry: `lib/saluki-io/src/deser/codec/dogstatsd/event.rs:31` `parse_dogstatsd_event`.
  - Length-prefixed body: `event.rs:36-49` parses `_e{<title_len>,<text_len>}:` then
    `take(title_len)` / `take(text_len)` — **attacker-controlled lengths**. nom `take` on a short
    buffer returns `Err`, not a panic (good), but this is the untrusted-length surface to fuzz.
  - UTF-8 validation + `.replace("\\n","\n")` allocation: `event.rs:51-59` (per-packet heap alloc
    keyed on attacker length — memory-amplification angle under flood).
  - Extension chunk loop: `event.rs:79-156` — `all_consuming` sub-parsers for `d:`(timestamp),
    `h:`,`k:`,`p:`,`s:`,`t:`,`c:`,`e:`,`card:`,`#tags`. `unix_timestamp` parser at
    `event.rs:88`; cardinality at `event.rs:133-139`. Unknown chunks silently skipped
    (`event.rs:145-149`, TODO: "throw an error, warn, or be silently permissive?").
- Service-check codec entry: `lib/saluki-io/src/deser/codec/dogstatsd/service_check.rs:28`
  `parse_dogstatsd_service_check`.
  - `_sc|<name>|<status:u8>` via `parse_u8` + `CheckStatus::try_from` (`service_check.rs:31-38`).
  - Extension loop `service_check.rs:48-104`: `d:`,`h:`,`c:`,`e:`,`#tags`,`m:`(utf8 message),`card:`.
    Same silent-skip TODO (`service_check.rs:93-97`).
- Decode dispatch + error counting: `lib/saluki-components/src/sources/dogstatsd/mod.rs:1462-1474`
  (`handle_frame` → `codec.decode_packet`); decode failure increments
  `event_decode_failed()` / `service_check_decode_failed()` (`mod.rs:1468-1469`,
  counters defined `sources/dogstatsd/metrics.rs:58-63`) and returns `Err(ParseError)`.
- Socket-survival mechanism (shared with metrics): connectionless framing/parse errors are
  logged + the buffer cleared + loop continues (`sut-analysis.md` §2, `mod.rs:1283-1318`); a bad
  event/SC frame must not kill the socket or process.

## Failure scenario
An adversarial event/SC payload triggers a panic or unbounded resource use in the dedicated codec
(e.g. a parser path the non-exhaustive unit tests miss: pathological length prefixes, invalid UTF-8
in title/text/message, malformed `d:` timestamp, `card:` parsing, multibyte boundary in
`.replace`). Because these codecs are entirely separate from the metric codec, the existing
`malformed-dsd-no-crash` coverage gives no assurance here. A panic on any DSD listener thread is a
process crash (data-plane components are fail-stop, `sut-analysis.md` §2); a crash-loop under a
malformed-event flood violates the headline "won't crash under load" guarantee.

## Observations
- Both codecs return `nom::Err` on bad input rather than panicking in the paths read; no `unwrap`/
  `expect`/`unsafe` was seen in `event.rs` or `service_check.rs` themselves. The risk is (a) shared
  helper parsers (`helpers::*` — `unix_timestamp`, `tags`, `cardinality`, `ascii_alphanum_and_seps`,
  `local_data`, `external_data`) and (b) the `.replace` allocations under flood. Helpers were not
  fully read — see Open Questions.
- `title_len == 0 || text_len == 0` is rejected (`event.rs:44-46`), but huge declared lengths just
  fail the `take` — confirm no pre-allocation on the declared length.
- Error policy for unknown trailing chunks is undecided (silently permissive) in BOTH codecs — same
  open policy question as the metric path; affects expected-drop accounting, not no-crash.

## Suggested assertions (MISSING / net-new — no Antithesis SDK in tree per `existing-assertions.md`)
- `Always(process_up)` and `Always(connectionless socket survives a bad event/SC packet)` — extends
  the metric-only `malformed-dsd-no-crash` to the event/SC frames; can reuse the same process-up /
  socket-survival workload checker but MUST drive event/SC frames specifically.
- SUT-side `Unreachable` at any panic site reachable from `parse_dogstatsd_event` /
  `parse_dogstatsd_service_check` and their helpers (none confirmed yet — guards regressions and the
  shared-helper risk).
- `Sometimes(event_decode_failed > 0)` and `Sometimes(service_check_decode_failed > 0)` — reachability
  anchors proving the malformed-event/SC parse-error paths are actually exercised (avoids vacuity;
  these counters already exist at `metrics.rs:58-63`).

## Config dependencies
- DogStatsD enabled (`data_plane.enabled: true`); events/service_checks sub-pipelines are on by
  default (`EnablePayloadsConfiguration` defaults `events: true`, `service_checks: true`,
  `sources/dogstatsd/mod.rs:205-221`).
- `client_origin_detection` gates the `c:`/`e:`/`card:` extension parsers (`event.rs:122-139`,
  `service_check.rs:68-92`); toggling it changes which parser branches untrusted bytes reach. Drive
  BOTH settings to cover the gated parsers.

## SUT-side instrumentation needs
- A process-up / socket-alive workload checker (shared with `malformed-dsd-no-crash`) plus
  event/SC-shaped malformed frames in the workload generator.
- The two `Sometimes` anchors read existing decode-failure counters; the `Unreachable` panic guard,
  if added, needs an SDK assertion compiled into the codec/helpers (net-new dependency).

## Open Questions
- Do the shared `helpers::*` parsers (`unix_timestamp`, `tags`, `cardinality`, `local_data`,
  `external_data`, `ascii_alphanum_and_seps`, `utf8`) contain any panic/`unwrap`/pre-allocation on
  attacker-controlled length? Not yet read — pivotal for whether the `Unreachable` guard is needed.
- Does `take(title_len)`/`take(text_len)` (or the message `utf8` parser) ever pre-allocate on the
  declared length before validating the buffer is long enough (a memory-amplification vector under
  flood)?
- Is a malformed event/SC frame ever mis-classified by `parse_message_type` (`mod.rs:1466`) such that
  the wrong decode-failure counter increments — cosmetic, but affects the `Sometimes` anchor wiring.
