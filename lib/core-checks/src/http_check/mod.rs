pub mod check;
pub mod config;
pub mod version;

pub use anyhow::{anyhow, bail, Result};
pub type GenericError = anyhow::Error;

pub use check::HttpCheck;
