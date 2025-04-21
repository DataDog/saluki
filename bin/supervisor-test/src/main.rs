use std::{future::pending, pin::Pin, prelude::rust_2024::Future, time::Duration};

use saluki_app::prelude::{fatal_and_exit, initialize_logging};
use saluki_core::runtime::supervisor::{Supervisable, Supervisor};
use saluki_error::GenericError;
use tracing::{error, info};

#[tokio::main]
async fn main() {
    if let Err(e) = initialize_logging(None) {
        fatal_and_exit(format!("failed to initialize logging: {}", e));
        return;
    };

    let mut supervisor = Supervisor::new("root");
    supervisor.add_process(WithDelay::never());
    supervisor.add_process(WithDelay::panic(Duration::from_secs(9)));

    let mut nested_supervisor = Supervisor::new("nested");
    nested_supervisor.add_process(WithDelay::success(Duration::from_secs(6)));
    nested_supervisor.add_process(WithDelay::failure("failed", Duration::from_secs(15)));
    supervisor.add_process(nested_supervisor);

    match supervisor.run().await {
        Ok(()) => info!("Supervisor completed successfully."),
        Err(e) => error!("Supervisor failed: {}", e),
    }
}

struct WithDelay {
    result: Result<(), &'static str>,
    panic: bool,
    delay: Duration,
}

impl WithDelay {
    fn success(delay: Duration) -> Self {
        Self {
            result: Ok(()),
            panic: false,
            delay,
        }
    }

    fn failure(msg: &'static str, delay: Duration) -> Self {
        Self {
            result: Err(msg),
            panic: false,
            delay,
        }
    }

    fn panic(delay: Duration) -> Self {
        Self {
            result: Ok(()),
            panic: true,
            delay,
        }
    }

    fn never() -> Self {
        Self {
            result: Ok(()),
            panic: false,
            delay: Duration::from_secs(0),
        }
    }
}

impl Supervisable for WithDelay {
    fn initialize(&self) -> Option<Pin<Box<dyn Future<Output = Result<(), GenericError>> + Send>>> {
        let delay = self.delay;
        let result = self.result.clone();
        let panic = self.panic;
        Some(Box::pin(async move {
            if delay.is_zero() {
                pending::<()>().await;
            } else {
                tokio::time::sleep(delay).await;
            }

            if panic {
                panic!("panic boom boom boom");
            }

            result.map_err(|e| GenericError::msg(e))
        }))
    }
}
