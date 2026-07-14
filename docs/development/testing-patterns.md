# Testing patterns

This page covers how we write unit and property tests inside the Rust codebase itself: naming, structure, fixtures,
mocking, and a handful of anti-patterns we've found repeated across the codebase. It's a companion to
[Testing](./testing.md) and [Style guide](./style-guide.md), not a replacement for either.

## Scope

This page is about the tests that live next to the code they exercise: `#[test]`/`#[tokio::test]` functions in `mod
tests` blocks and `proptest!`/`#[test_strategy::proptest]` property tests, across every crate in `lib/` and `bin/`. It
also covers `tests/` integration-test files within a crate (for example, `saluki-tls`'s per-file integration tests).

It does **not** cover:

- Loom tests, Miri tests, Antithesis tests, correctness tests (panoramic), integration tests (panoramic), or SMP
  benchmark experiments: see [Testing](./testing.md) for the strategy and tooling behind each of those.
- Rustdoc conventions, Markdown formatting, or the `make fmt`/`make check-clippy` mechanical rules: those still apply in
  full to test code, and live in the [Style guide](./style-guide.md).

Everything below assumes you've already read the [Style guide](./style-guide.md)'s requirement-level conventions
(**MUST**/**SHOULD**/**MAY**) and voice guidance; we use the same conventions here.

## Test module and function naming

Follow these high-level conventions when writing basic tests:

- Tests **SHOULD** be declared in a module called `tests`, gated by the `#[cfg(test)]` attribute. For example:
  ```rust
  #[cfg(test)]
  mod tests {
      
  }
  ```
- Prefer a bare, descriptive sentence as the test function name. Do not add the `test_` prefix to the function name. For
  example, `metric_host_tag_disambiguates_contexts_without_remaining_tag`.
- When a source file has a differing convention for test function naming, and you're making changes to that file, you
  **SHOULD** update existing tests to conform to these guidelines.
- When adding a single-purpose integration test file under a crate's `tests/` directory, the file name **SHOULD**
  reflect our "bare, descriptive sentence" test function naming convention, and the name the file's sole test function
  **SHOULD** be the same as the file's stem. For example, `partial_load_succeeds.rs` should have a test function named
  `partial_load_succeeds`.

## Property-based tests

Two decisions govern how you write a proptest in this repository: the prefix, and the authoring style.

> [!IMPORTANT]
> Every property-based test function **must** be named with a `property_test_` prefix, with no exceptions. This
> isn't a style preference: our test-related Make targets pick which tests to run based on including/excluding tests
> with the `property_test_` prefix.

Two authoring styles for the test body coexist in the codebase: the classic `proptest! { ... }` block macro (used in
roughly a dozen files) and the `#[test_strategy::proptest(async = "tokio")]` attribute macro. You **SHOULD** default to
the classic block macro. Reach for the attribute macro style only when the property test itself needs to be `async` (for
example, because it drives a Tokio-based component under test) since that's the concrete problem the attribute style
solves that the block macro doesn't.

Property test **SHOULD** contain at least one `prop_assert!`/`prop_assert_eq!` that encodes a real invariant. A proptest
with no assertion at all can only ever fail via an internal panic, never by catching a genuinely violated property. Some
property tests may have no assertions at all, used in quasi-fuzz test fashion, but this is a legacy exception and not
the norm.

For a complex stateful structure, the strongest form is a differential/model-based test: replay an identical random
operation sequence against both the real implementation and a deliberately naive reference mode.

## Table-driven tests

When you find yourself writing a second (or third, or twentieth) test function that repeats the same setup and assertion
shape and differs only in one input value and its expected output, collapse the group into a single test that iterates
over a small array of cases instead of adding another near-duplicate function.

**Exercise restraint**, though: for only two or three closely related scenarios, a single test with a few named local
variables or sequential assertions against shared setup is simpler and just as clear. Reach for a full blown "array of
test cases" loop once you have four or more near-identical variants, or when the case table itself documents something
worth naming: for example, porting a reference implementation's own test table.

## Assertion density

A test that cannot fail regardless of a real correctness regression provides no protection, and it actively misleads
a reviewer into thinking the behavior is covered. Watch for these shapes when you write or review a test:

- **Zero assertions.** In some cases, we write tests to easily introspect something about the codebase, such as printing
  the size of a particular data structure. These tests **MUST** be marked as ignored (`#[ignore]`) so they don't interfere
  with the test suite's normal execution.
- **Tautologies.** A test that calls a pure function twice with the same input and asserts the two results are equal
  proves only that the function is deterministic, not that it's correct. These are OK if determinism is the intended
  property to test, but the test function should be named to reflect this intent.
- **Silently skipped assertions.** `if let Some(x) = maybe_thing() { assert!(...) }` with no `else`/`expect` branch
  means a regression that makes `maybe_thing()` always return `None` causes the assertion to never execute... and the
  test still passes. Prefer full match blocks (`match maybe_thing() { ... }`, or `if let Some(x) = maybe_thing() { ... }
  else { ... }`) to avoid silent failures.
- **Weak bound-only assertions.** We **SHOULD** prefer assertions that check not only quantitatively but qualitatively.
  For example, checking that a cache implementation properly evicts items when the cache is full can be done just by
  checking that the cache length is less than or equal to the maximum capacity. However, such a check doesn't address
  whether or not the _intended_/_expected_ items were evicted. These two things _can_ be split into separate test
  functions, but it is often more concise/succinct to check them together.
- **Shallow success-only checks.** Asserting `.is_some()`/`.is_ok()` on a value the documentation specifies precisely (a
  percentage, a byte count, a status mapping) throws away the one thing worth testing. If the doc comment gives you an
  exact expected number, assert that number.

### Match tests to documented behavior

When a function's doc comment enumerates specific branches, error variants, or worked examples, treat that enumeration
as your test checklist, not just a description. The most common coverage gap for highly complex code is only testing the
happy path, not the error cases or edge conditions. A good test module reads as a direct, traceable map of its target's
documented contract

When you write or review a doc comment with an `# Errors` section, a branch list, or a worked example, check off each
item against an actual test before you consider the function covered.

## Shared test fixtures belong behind a Cargo feature, not `#[cfg(test)]`

When you write a constructor, mock, or fixture builder that other test modules (possibly in other crates) will
need, don't gate it with `#[cfg(test)]`. `#[cfg(test)]` only compiles when *that crate itself* is built as a test
target; it's invisible to every downstream crate. The predictable result is that each consumer hand-rolls an
equivalent construction instead of reusing yours, and the duplicates quietly drift apart from each other.

Instead, create a crate feature, `test-util`, which _additionally_ exposes the helper on top of just `cfg(test)`,
like so:

```rust
#[cfg(any(test, feature = "test-util"))]
pub fn some_helper_function(...) {}
```

> [!IMPORTANT]
> Before you write `#[cfg(test)] pub fn make_foo(...)`, ask whether another crate might need the same fixture. If the
> answer is "maybe," gate it behind a non-default Cargo feature instead (conventionally named `test`) and expose it
> as a plain `pub` item, so downstream crates can pull it in as a `dev-dependency` with that feature enabled.

## Prefer small named helpers over repeated inline setup

When two or more tests in the same file build the same fixture, compare the same shape, or drive the same multi-step
harness, extract a small, well-named helper rather than repeating the code inline. This comes in many forms:

- **Builder helpers** that take only the parameters that vary.
- **Comparison helpers** that hide an equality check's irrelevant details.
- **End-to-end harness helpers** that collapse a multi-step spawn/push/drain sequence into one call.
- **`#[track_caller]` assertion helpers** for consistent, useful panic locations.
- **Loader helpers** that wrap config parsing for a whole test suite.
- **Configurable scenario harnesses** that replace a family of near-duplicate test doubles with one parameterized one.

> [!NOTE]
> When dealing with a large number of near-identical checks, you can reach for a domain-specific assertion macro over
> ad-hoc `assert_eq!` calls.

Don't over-apply this: for a small pure function with only a handful of branches, one plain test walking every branch
inline is clearer than introducing a helper or a table for its own sake.

## Mocking philosophy

When your test suite owns both the producer and the consumer of a protocol, wire format, or routing table, prefer
driving the real implementation on both sides over mocking one side or assuming a byte layout. A mock only tests that
your code agrees with your own assumptions about the dependency; a real round-trip tests that your code actually
works. Reserve mocks and test doubles for the specific branch or failure mode that real infrastructure genuinely cannot
trigger deterministically, but not as a default first choice.

## Regression tests and ported tests should name their source

When you add a regression test for a bug you just fixed, write a comment that names the exact historical failure mode,
the code path, and the fix... don't _just_ add the assertion. A future reader (including future _you_) needs that context
to know whether a refactor might reintroduce the bug.

When you port a test from an upstream reference implementation (the Go trace agent, a reference DDSketch implementation,
and so on), say so explicitly in a comment, ideally with a pinned link:. This provenance comment is what lets a future
maintainer tell the difference between "this test looks arbitrary" and "this test encodes a specific upstream contract
we must not silently diverge from."

## Ignore and perf-test policy

**Do not** add `#[ignore]`'d `#[test]`/`#[tokio::test]` functions that time production code and only `println!` the
result. An ignored, assertion-free test can never fail regardless of a real performance regression, it doesn't get a
statistical baseline or regression threshold, and it invites duplication.

You **SHOULD** write a proper benchmark, whether through an actual benchmarking crate like `criterion` or through
[SMP](./testing.md#benchmark-tests-single-machine-performance-smp) for full-system throughput/memory regressions.
