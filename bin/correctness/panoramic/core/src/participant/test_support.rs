//! Executes participant operations with scripted commands or local HTTP routes.

use super::*;
use crate::{
    endpoint::{EndpointKind, ResolvedAddress},
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
                requests: BTreeMap::new(),
            },
            args,
        )
        .await
}

/// Runs an operation with its declared HTTP endpoints routed to a loopback port and a ten-second limit.
///
/// # Errors
///
/// Returns the operation's error unchanged.
///
/// # Panics
///
/// Panics if the operation uses a participant primitive instead of HTTP.
pub async fn run_http_operation(operation: &dyn Operation, args: Value, port: u16) -> Result<Value, HandleError> {
    let primitives = ScriptedExec(|_: &ExecCommand| panic!("the operation must use HTTP"));
    let requests = operation
        .http_endpoints()
        .iter()
        .map(|&endpoint| {
            (
                endpoint,
                RunnerHostRoute::Direct(ResolvedAddress {
                    kind: EndpointKind::Http,
                    url: format!("http://127.0.0.1:{port}"),
                    host: Some("127.0.0.1".into()),
                    port: Some(port),
                    path: None,
                }),
            )
        })
        .collect();
    operation
        .run(
            &OpContext {
                primitives: &primitives,
                limit: Duration::from_secs(10),
                requests,
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
