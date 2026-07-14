# Differential scenario

This scenario tests that the Datadog Agent emits the same metric contexts for the
same DogStatsD input whether its embedded data plane (ADP) is off or on. Values
for contexts are not compared.

## How it works

This scenario comprises the following components:

* Datadog Agent, ADP-off
* Datadog Agent, ADP-on
* `intake`
* parallel drivers

Both lanes are the same converged Datadog Agent image, differing only in whether
`DD_DATA_PLANE_ENABLED` hands DogStatsD to ADP. They are the systems under test,
driven from the same, equivalently applicable configuration. That is, a
configuration option that only affects one lane will not be present, as an
example. References below to "ADP" mean the data-plane-on lane and "Datadog
Agent" the data-plane-off lane.

The drivers emit into both SUTs. We take as 'transmitted' that the send has
reached kernel buffers and is 'durable'. This is the pattern we advertise to
customers. If a driver is able to write to one SUT but not another, the scenario
run is failed: all writes reach both SUTs or no conclusion can be
drawn. Contexts are created on-demand by drivers. We allow Antithesis randomness
to drive choices around total contexts per scenario. We intentionally craft
strange-but-valid dogstatsd metric lines -- called 'feral' -- in order to find
differences between the SUTs. The judgement criteria are:

* If Agent _rejects_ a line, ADP must _reject_ the line.
* If Agent _accepts_ a line, ADP must _accept_ the line.
* If ADP _rejects_ a line but Agent does not, ADP is deviated.
* If ADP _accepts_ a line but Agent does not, ADP is deviated.

The last two points are the negation of the first two, stated explicitly because
there is some ambiguity about symmetric relations. The consequence of this is
that equivalence is checked thusly. The `intake` registers writes from the two
SUTs based on their socket, which we call a 'lane'. When `intake` receives a
context on a lane the timestamp is also recorded. Within `intake` we compute a
set of contexts. These sets we call `C_ADP` and `C_DA` for 'cumulative ADP'
etc. Our goal is to determine if `C_ADP` and `C_DA` differ. Dogstatsd is
eventually consistent, that is, a context sent should _eventually_ be emitted
and our claim in this scenario is that this emission must occur within
`acceptable_flush_delay` seconds. Both SUTs flush on regular intervals but
without a guarantee of timeliness or of a reference 0-time. We sidestep this by
making assertions on the symmetric difference of `C_ADP` and `C_DA` and elements
entry and exit times.

The symmetric difference of two sets are those items present in one set or the
other but not _both_. In this scenario that means a context is missing from one
of the SUTs egress and has been dropped or name mangled or _something_. Let this
difference be a structure `D = (C_ADP - C_DA) U (C_DA - C_ADP)` and let `D`
record a `first_seen` timestamp for each member. For clarity, members `m` leave
`D` when `m` is a member of the intersection of `C_DA` and `C_ADP`. For every
member `m` of `D` let `delayed(m) = now - first_seen >
acceptable_flush_delay`. We compute `delayed` for each member of `D` in
`eventually_` checks. So long as `delayed(m) == false` for all `m` in `D` the
check passes. In the `finally_` check we sleep for `acceptable_flush_delay` and
then assert that `D == {}`.

# Caveats

1. This is a differential test. If ADP and Datadog Agent are buggy in the _same
   manner_ then this will be accorded a success. Consider for instance if both
   SUTs never emit any contexts incorrectly.
2. We are only making claims about when a context appears in output. We are not
   claiming that contexts are refreshed correctly. So, for instance, if one of
   the systems only ever emits one instance of a continually updated context
   this will not be detected.
3. `acceptable_flush_delay` must be set by convention.

# Assumptions

1. We assume that if a context is emitted by either SUT it will be emitted in
   perpetuity. That is, if an `m` leaves `D` it will not return. We DO NOT check
   this assumption, although we may choose to in the future.
2. We assume that `finally_` checks run after drivers are completed, faults. We
   assume that `eventually_` checks run mid-scenario but not during driver
   emission, faults.
3. We exclude `datadog.*` from calculations for `D`.
4. A metric context is defined as the 3-tuple `(metric_name, tagset, kind)`
   where tagset is a set of `(key, value)` pairs and must be compared for
   equality as a set, not a string. That is, order _does not matter_ in the
   tagset.
