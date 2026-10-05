#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
pub(crate) use linux::{wait, wake};
#[cfg(target_os = "macos")]
pub(crate) use macos::{wait, wake};
