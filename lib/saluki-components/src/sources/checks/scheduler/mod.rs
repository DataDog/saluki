use std::sync::Arc;

use tokio::sync::mpsc;

pub use super::check::Check;

pub mod job;
pub mod runner;
pub mod scheduler;
pub mod tracker;
pub mod worker;

pub use runner::Runner;
pub use scheduler::Scheduler;

pub type CheckReceiver = mpsc::Receiver<Arc<dyn Check + Send + Sync>>;
pub type CheckSender = mpsc::Sender<Arc<dyn Check + Send + Sync>>;
