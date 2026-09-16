//! Records the Git revision this binary was built from, for the machine-readable test reports.
//!
//! `vergen-gix` reads the repository through `gix`, so no `git` executable has to exist on the
//! machine doing the build.

use vergen_gix::{Emitter, GixBuilder};

fn main() {
    // Best effort: a build outside a checkout, such as from a source tarball, still has to succeed.
    // The binary reports these fallback values as an unknown revision.
    if let Err(e) = emit_revision() {
        println!("cargo:warning=Could not read the Git revision: {}", e);
        println!("cargo:rustc-env=VERGEN_GIT_SHA=");
        println!("cargo:rustc-env=VERGEN_GIT_DIRTY=false");
    }
}

/// Emits the SHA and dirty state of the repository this crate is being built from.
fn emit_revision() -> Result<(), Box<dyn std::error::Error>> {
    let gix = GixBuilder::default().sha(false).dirty(true).build()?;
    Emitter::default().add_instructions(&gix)?.emit()?;
    Ok(())
}
