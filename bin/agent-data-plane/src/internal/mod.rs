use std::future::Future;

use saluki_app::metrics::collect_runtime_metrics;
use saluki_error::{generic_error, ErrorContext as _, GenericError};

mod control_plane;
pub use self::control_plane::spawn_control_plane;

mod observability;
pub use self::observability::spawn_internal_observability_topology;

mod remote_agent;

/// Creates a single-threaded Tokio runtime, initializing it and driving it to completion.
///
/// A dedicated background thread is spawned on which the runtime executes. The `init` future is run within the context
/// of the runtime and is expected to return a `Result<T, GenericError>` that indicates that initialization has either
/// succeeded or failed. `main_task` is used to create the future which the runtime will ultimately drive to completion.
///
/// If initialization succeeds, `main_task` is called the result from `init` to create the main task future, and this
/// function returns `Ok(())`. If initialization fails, this function returns `Err(e)`.
///
/// # Errors
///
/// If the current thread runtime cannot be created, or the background thread for the runtime cannot be created, or an
/// error is returned from the execution of `init`, an error will be returned.
fn initialize_and_launch_runtime<F, T, F2, T2>(name: &str, init: F, main_task: F2) -> Result<(), GenericError>
where
    F: Future<Output = Result<T, GenericError>> + Send + 'static,
    F2: FnOnce(T) -> T2 + Send + 'static,
    T2: Future<Output = ()>,
{
    let mut builder = tokio::runtime::Builder::new_current_thread();
    let runtime = builder
        .enable_all()
        .max_blocking_threads(2)
        .build()
        .error_context("Failed to build current thread runtime.")?;

    let runtime_id = name.to_string();
    let (init_tx, init_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            // Run the initialization routine within the context of the runtime.
            match runtime.block_on(init) {
                Ok(init_value) => {
                    // Initialization succeeded, so inform the main thread that the runtime has been initialized and
                    // will continue running, and pass whatever we got back from initialization and drive the main
                    // task to completion.
                    init_tx.send(Ok(())).unwrap();

                    // Start collecting runtime metrics.
                    runtime.spawn(async move {
                        collect_runtime_metrics(&runtime_id).await;
                    });

                    runtime.block_on(main_task(init_value));
                }
                Err(e) => {
                    // Initialization failed, so send the error back to the main thread.
                    init_tx.send(Err(e)).unwrap();
                }
            }
        })
        .with_error_context(|| format!("Failed to spawn thread for runtime '{}'.", name))?;

    // Wait for the initialization to complete and forward back the result if we get one.
    match init_rx.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(generic_error!(
            "Initialization result channel closed unexpectedly. Runtime likely in an unexpected/corrupted state."
        )),
    }
}
