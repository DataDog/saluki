use std::collections::HashMap;

pub type GenericError = anyhow::Error;
pub type Result<T> = anyhow::Result<T>;

pub type Mapping = serde_yaml::value::Mapping;

pub type Tags = HashMap<String, String>;

pub mod check;
pub mod event;
pub mod event_platform;
pub mod histogram;
pub mod log;
pub mod metric;
pub mod service_check;
pub mod sink;
