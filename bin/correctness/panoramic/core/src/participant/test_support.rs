//! Executes participant operations with a scripted command primitive.

use super::*;
use crate::{
    endpoint::ResolvedAddress,
    handle::FileStatus,
    probe::{AttemptLimits, ProbeAnswer},
};

/// Runs an operation with scripted command output and a ten-second execution limit.
///
/// # Errors
///
/// Returns the operation's error unchanged.
pub async fn run_operation(
    operation: &dyn Operation, args: Value,
    exec: impl Fn(&ExecCommand) -> Result<ExecOutput, HandleError> + Send + Sync,
) -> Result<Value, HandleError> {
    let primitives = ScriptedExec(exec);
    operation
        .run(
            &OpContext {
                primitives: &primitives,
                limit: Duration::from_secs(10),
            },
            args,
        )
        .await
}

struct ScriptedExec<F>(F)
where
    F: Fn(&ExecCommand) -> Result<ExecOutput, HandleError> + Send + Sync;

#[async_trait]
impl<F> Primitives for ScriptedExec<F>
where
    F: Fn(&ExecCommand) -> Result<ExecOutput, HandleError> + Send + Sync,
{
    async fn exec(&self, command: ExecCommand, _: Duration) -> Result<ExecOutput, HandleError> {
        (self.0)(&command)
    }

    async fn read_file(&self, _: &str) -> Result<Option<Vec<u8>>, HandleError> {
        panic!("the operation only executes commands");
    }

    async fn stat(&self, _: &str) -> Result<Option<FileStatus>, HandleError> {
        panic!("the operation only executes commands");
    }

    async fn probe(&self, _: &ResolvedAddress, _: &AttemptLimits) -> Result<ProbeAnswer, HandleError> {
        panic!("the operation only executes commands");
    }

    async fn send(&self, _: &ResolvedAddress, _: &[u8], _: &AttemptLimits) -> Result<(), HandleError> {
        panic!("the operation only executes commands");
    }
}
