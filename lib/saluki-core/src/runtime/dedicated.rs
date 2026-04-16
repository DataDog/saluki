//! Dedicated runtime support for supervisors.

use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{ready, Context, Poll},
    thread::JoinHandle,
};

use saluki_error::{generic_error, GenericError};
use tokio::sync::oneshot;

use super::{
    shutdown::ProcessShutdown,
    state::DataspaceRegistry,
    supervisor::{Supervisor, SupervisorError},
};

/// Configuration for a dedicated Tokio runtime.
#[derive(Clone, Debug)]
pub struct RuntimeConfiguration {
    /// Number of worker threads.
    worker_threads: usize,
}

impl RuntimeConfiguration {
    /// Creates a new single-threaded `RuntimeConfiguration`.
    ///
    /// The underlying executor implementation is based on `tokio`'s "current thread" runtime.
    pub const fn single_threaded() -> Self {
        Self { worker_threads: 1 }
    }

    /// Creates a new multi-threaded `RuntimeConfiguration` with the given number of worker threads.
    ///
    /// The underlying executor implementation is based on `tokio`'s multi-threaded runtime.
    pub const fn multi_threaded(worker_threads: usize) -> Self {
        Self { worker_threads }
    }

    /// Builds the Tokio runtime from this configuration.
    pub(crate) fn build(&self, supervisor_id: &str) -> io::Result<tokio::runtime::Runtime> {
        let supervisor_id = supervisor_id.to_string();
        let thread_id = Arc::new(AtomicUsize::new(0));

        if self.worker_threads == 1 {
            tokio::runtime::Builder::new_current_thread().enable_all().build()
        } else {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .enable_eager_driver_handoff()
                .enable_alt_timer()
                .worker_threads(self.worker_threads)
                .thread_name_fn(move || {
                    let new_thread_id = thread_id.fetch_add(1, Ordering::SeqCst);
                    format!("{}-sup-{:02}", supervisor_id, new_thread_id)
                })
                .build()
        }
    }
}

/// Controls which runtime a supervisor runs on.
#[derive(Clone, Debug, Default)]
pub enum RuntimeMode {
    /// Run on the ambient runtime (default).
    ///
    /// The supervisor runs on whatever Tokio runtime is currently active when it is spawned.
    #[default]
    Ambient,

    /// Run on a dedicated runtime with the given configuration.
    ///
    /// The supervisor spawns its own OS thread(s) and Tokio runtime, providing runtime isolation from the parent
    /// supervisor.
    Dedicated(RuntimeConfiguration),
}

/// A handle to a supervisor running in a dedicated runtime.
///
/// Allows capturing any runtime initialization failures as well as the result of the supervisor's execution.
pub(crate) struct DedicatedRuntimeHandle {
    supervisor_id: String,
    init_rx: Option<oneshot::Receiver<Result<(), GenericError>>>,
    result_rx: oneshot::Receiver<Result<(), SupervisorError>>,
    thread_handle: Option<JoinHandle<()>>,
}

impl Future for DedicatedRuntimeHandle {
    type Output = Result<(), SupervisorError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // First, check if initialization is still pending.
        //
        // NOTE: This is runtime-level initialization (building the Tokio runtime and OS thread), not
        // supervisor child initialization. These errors are always fatal and non-restartable.
        if let Some(init_rx) = self.init_rx.as_mut() {
            let init_result = ready!(Pin::new(init_rx).poll(cx));
            let maybe_init_error = match init_result {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e),
                Err(_) => Some(generic_error!(
                    "no initialization result received; runtime creation likely panicked"
                )),
            };

            self.init_rx = None;

            if let Some(error) = maybe_init_error {
                // Join on the thread to clean up.
                if let Some(handle) = self.thread_handle.take() {
                    let _ = handle.join();
                }

                return Poll::Ready(Err(SupervisorError::FailedToInitialize {
                    child_name: self.supervisor_id.clone(),
                    source: error.into(),
                }));
            }
        }

        // Check for a final result from the supervisor. The structured `SupervisorError` is preserved
        // so the parent can distinguish init failures from runtime failures.
        //
        // If the channel is closed without a result, the runtime thread panicked or exited after
        // successful init — this is a runtime failure, not an initialization failure.
        let result = ready!(Pin::new(&mut self.result_rx).poll(cx)).unwrap_or_else(|_| Err(SupervisorError::Shutdown));

        // Join on the thread to clean up.
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }

        Poll::Ready(result)
    }
}

/// Spawns a supervisor in a dedicated runtime on a new OS thread, returning a handle to await for the result.
///
/// The returned handle will resolve if initialization of the runtime failed, or after the supervisor completes,
/// whether due to shutdown or failure.
///
/// A background OS thread is spawned to run the supervisor, named after the supervisor's ID: `<id>-sup-rt`. For
/// multi-threaded runtimes, additional threads will be spawned following a similar naming convention:
/// `<id>-sup-[0-9]+`.
///
/// # Errors
///
/// If the OS thread cannot be spawned, an error is returned.
pub(crate) fn spawn_dedicated_runtime(
    mut supervisor: Supervisor, config: RuntimeConfiguration, process_shutdown: ProcessShutdown,
    dataspace: DataspaceRegistry,
) -> Result<DedicatedRuntimeHandle, GenericError> {
    let (init_tx, init_rx) = oneshot::channel();
    let (result_tx, result_rx) = oneshot::channel();

    let supervisor_id = supervisor.id().to_string();
    let thread_name = format!("{}-sup-rt", supervisor_id);
    let thread_handle = std::thread::Builder::new()
        .name(thread_name.clone())
        .spawn(move || {
            // Build the runtime.
            let runtime = match config.build(supervisor.id()) {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = init_tx.send(Err(generic_error!("Failed to build dedicated runtime: {}", e)));
                    return;
                }
            };

            // Signal that initialization succeeded.
            if init_tx.send(Ok(())).is_err() {
                // Parent is no longer listening, bail out.
                return;
            }

            // Run the supervisor to completion and send the result back.
            //
            // We pass the parent's dataspace so the nested supervisor inherits it across the
            // thread boundary rather than creating a new one.
            let result = runtime.block_on(supervisor.run_with_process_shutdown(process_shutdown, Some(dataspace)));
            let _ = result_tx.send(result);
        })
        .map_err(|e| generic_error!("Failed to spawn dedicated runtime thread '{}': {}", thread_name, e))?;

    Ok(DedicatedRuntimeHandle {
        supervisor_id,
        init_rx: Some(init_rx),
        result_rx,
        thread_handle: Some(thread_handle),
    })
}
