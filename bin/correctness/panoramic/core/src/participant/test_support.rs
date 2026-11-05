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
    run_operation_with_process(operation, args, exec, |_, _| panic!("the operation must use exec")).await
}

/// Runs an operation with scripted command and declared-process output and a ten-second execution limit.
///
/// # Errors
///
/// Returns the operation's error unchanged.
pub async fn run_operation_with_process(
    operation: &dyn Operation, args: Value,
    exec: impl Fn(&ExecCommand) -> Result<ExecOutput, HandleError> + Send + Sync,
    exec_process: impl Fn(&str, &[String]) -> Result<ExecOutput, HandleError> + Send + Sync,
) -> Result<Value, HandleError> {
    let primitives = ScriptedExec(exec, exec_process);
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
    let primitives = ScriptedExec(
        |_: &ExecCommand| panic!("the operation must use HTTP"),
        |_: &str, _: &[String]| panic!("the operation must use HTTP"),
    );
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

struct ScriptedExec<F, G>(F, G);

#[async_trait]
impl<F, G> Primitives for ScriptedExec<F, G>
where
    F: Fn(&ExecCommand) -> Result<ExecOutput, HandleError> + Send + Sync,
    G: Fn(&str, &[String]) -> Result<ExecOutput, HandleError> + Send + Sync,
{
    async fn exec(&self, command: ExecCommand, _: Duration) -> Result<ExecOutput, HandleError> {
        (self.0)(&command)
    }

    async fn exec_process(&self, process: &str, args: Vec<String>, limit: Duration) -> Result<ExecOutput, HandleError> {
        assert_eq!(limit, Duration::from_secs(10));
        (self.1)(process, &args)
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
    async fn http(
        &self, _: &ResolvedAddress, _: &str, _: bool, _: &AttemptLimits,
    ) -> Result<HttpResponse, HandleError> {
        panic!("the operation expects HTTP requests to use a direct route");
    }
}
