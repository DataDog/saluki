//! Shared write path for generated files that `rustfmt` formats.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Format `contents` with `rustfmt` and write it to `path` if the result differs from what is
/// already there.
///
/// Formatting happens before the comparison on purpose. Formatting the file after writing it
/// leaves `path` holding something the generator never emits, so every build sees a difference,
/// rewrites the file, and gives it a new modification time. That defeats `rerun-if-changed` on the
/// output directory: cargo re-runs the build script, which rewrites the file again.
///
/// A missing or failing `rustfmt` is not fatal; the raw output is written instead.
pub fn write_formatted(path: &Path, contents: &str) {
    let formatted = rustfmt(contents).unwrap_or_else(|| contents.to_string());

    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing == formatted {
        return;
    }
    std::fs::write(path, &formatted).unwrap_or_else(|e| panic!("cannot write {}: {}", path.display(), e));
}

/// Run `contents` through `rustfmt`, returning `None` if it cannot be run or reports failure.
fn rustfmt(contents: &str) -> Option<String> {
    let mut child = Command::new("rustfmt")
        .arg("--edition")
        .arg("2021")
        .arg("--emit")
        .arg("stdout")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;

    child.stdin.take()?.write_all(contents.as_bytes()).ok()?;

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}
