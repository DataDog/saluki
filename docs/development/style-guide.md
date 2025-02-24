# Style Guide

Our style guide focuses on two main areas: **formatting** and **aesthetics**.

While these are generally intertwined, formatting is almost entirely mechanical: we can consistently format code with
tools, reducing the mental burden on developers. Aesthetics, however, are more subjective and can't be easily automated.

## Formatting

Formatting primarily includes the formatting of Rust _code_, but we refer to it here more broadly: any mechanical
transformation that we can automate around the structure or ordering of code, configuration, and so on. We do this in
service on making the codebase consistent and easier to read, but also to reduce the mental overhead of worrying about
the styling of code as it's being written, as well as dealing with needless debate over _how_ to style. Developers run
the formatting tools, and the tools apply certain rules to the codebase.

Developers **MUST** use the **fmt** Make target (`make fmt`) to run all of the configured formatting operations on the
codebase as a whole. This target includes:

- running `cargo fmt` on the code (customized via `rustfmt.toml`)
- running `cargo autoinherit` to promote the use of shared workspace dependencies
- running `cargo sort` to sort dependencies within the `Cargo.toml` files

These formatters are checked and enforced in CI, so it's best to get in the habit of running `make fmt` locally before
committing changes.

Additionally, we use `cargo clippy` to catch common mistakes and suboptimal code patterns. Clippy will generally show
suggestions of more idiomatic or succinct ways to write common code patterns, as well as potentially point out subtle
bugs with certain usages. While Clippy _does_ support being able to automatically apply suggested fixes, doing so
requires a clean working copy (no uncommitted changes), and we opt not to override that behavior as it could potentially
overwrite changes that the developer has not yet fully worked through.

As such, developers **MUST** run `make check-clippy` and apply the suggestions. Like the formatters, Clippy lints are
checked and enforced in CI.

## Aesthetics

Aesthetics are the more subjective areas of how we write and structure code in Saluki. We'll talk about a number of
different items below, which range from very specific to more broad. We don't expect everyone to remember all of these,
especially since they're not easily automated, and so you might find us calling them out in PR reviews if missed.

### Generic types and type bounds

You'll often encounter types that use generics. These can vary in complexity and size, and can be applied on types,
trait implementations, method signatures, so on. In order to try and keep things clean and readable, we try to use
generics in the following way:

```rust
// Types and traits MUST use `where` clauses to introduce type parameters:
pub struct MyType<T>
where
    T: SomeTrait,
{
}

pub trait MyTrait<T>
where
	T: SomeTrait,
{
}

// Method signatures SHOULD use `where` clauses to introduce type parameters,
// but using `impl Trait` notation is also acceptable, especially when being
// used to convert arguments to concrete types:
fn my_method<F>(&self, frobulator: F) -> T
where
	F: Frobulator,
{
	frobulator.frobulate();
}

fn set_name(&mut self, name: impl Into<String>) {
	self.name = name.into();
}
```
