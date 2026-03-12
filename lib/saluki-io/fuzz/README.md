# Saluki-IO Fuzz Targets

This directory contains fuzzing targets for the `saluki-io` crate.

## Prerequisites

This requires rust nightly and `cargo-fuzz`.

```bash
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Run locally

```bash
# cargo-fuzz requires us to be in the specific folder
cd lib/saluki-io

cargo +nightly fuzz run dogstatsd_decode_packet

# or, with coverage enabled
cargo +nightly fuzz coverage dogstatsd_decode_packet
```

## Corpus and Artifacts

- Corpus: `fuzz/corpus/<target_name>/` - Contains interesting inputs discovered during fuzzing
- Artifacts: `fuzz/artifacts/<target_name>/` - Contains inputs that caused crashes or failures

## Viewing Coverage

After running with coverage:

```bash
cargo +nightly fuzz coverage dogstatsd_decode_packet
# Coverage report will be generated in fuzz/coverage/dogstatsd_decode_packet/
```
