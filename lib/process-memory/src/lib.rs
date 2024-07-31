//! Process memory querying.
//!
//! This crate provides a cross-platform way to query the RSS (resident set size) usage of a process.
//!
//! ## Linux
//!
//! On Linux, [procfs](https://docs.kernel.org/filesystems/proc.html) is used, and one of three files may be read,
//! depending on their availability:
//!
//! - `/proc/self/smaps_rollup`: This file is a pre-aggregated version of `/proc/self/smaps`, and is the most efficient way
//!   to query RSS. (Available in Linux 4.14+)
//! - `/proc/self/smaps`: This file contains detailed information about the memory mappings of the process, and can be
//!   aggregated to determine the resident set size. (Available in Linux 2.6.14+)
//! - `/proc/self/statm`: This file contains lazily-updated memory statistics about the process, and is the least
//!   accurate, but is generally good enough for most use-cases. (Available in Linux 2.6+)
//!
//! ## macOS
//!
//! On macOS, we query the kernel directly for Mach task information.
//!
//! Available since Mac OS X 10.0 (Cheetah).
//!
//! ## Windows
//!
//! No support yet.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::Querier;

#[cfg(target_os = "macos")]
mod darwin;

#[cfg(target_os = "macos")]
pub use darwin::resident_set_size;

#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
pub fn resident_set_size() -> Option<usize> {
    None
}
