# Sample metric values log-uniformly

## Problem

The value sampler emits about 19 fixed magnitudes. A DDSketch bins by
log-magnitude and only does anything interesting past 4096 distinct buckets. 19
magnitudes give 19 buckets, so every sketch anchor is dead on arrival.

## Solution

Draw the value log-uniformly across a wide range. Values spread evenly across
buckets instead of piling onto a handful.

## Implementation

```
exp   ~ uniform(-30, 30)
value = sign * 10^exp
```

Values run from 1e-30 to 1e30, about 8900 buckets at the agent's gamma, so 4096
is reached with headroom. Every band below is equally likely.

| value band | P |
| --- | --- |
| 1e-30 .. 1e-20 | 16.7% |
| 1e-20 .. 1e-10 | 16.7% |
| 1e-10 .. 1e0 | 16.7% |
| 1e0 .. 1e10 | 16.7% |
| 1e10 .. 1e20 | 16.7% |
| 1e20 .. 1e30 | 16.7% |
