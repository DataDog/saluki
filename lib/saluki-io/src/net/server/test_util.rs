#[cfg(unix)]
use std::{io::ErrorKind, path::Path};
use std::{sync::Mutex, time::Duration};

use async_trait::async_trait;
use saluki_common::sync::shutdown::ShutdownHandle;
use saluki_core::runtime::{
    state::{DataspaceRegistry, DataspaceUpdate, IdentifierFilter},
    InitializationError, Supervisable, Supervisor, SupervisorError, SupervisorFuture,
};
use saluki_error::generic_error;
#[cfg(unix)]
use tokio::{net::UnixStream, time::sleep};
use tokio::{sync::oneshot, task::JoinHandle, time::timeout};

use super::BoundServerAddress;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
pub async fn connect_unix(path: &Path) -> UnixStream {
    timeout(TEST_TIMEOUT, async {
        loop {
            match UnixStream::connect(path).await {
                Ok(stream) => return stream,
                Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
                    sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("should connect to Unix listener: {error}"),
            }
        }
    })
    .await
    .expect("should connect to Unix listener before timeout")
}

pub struct ServerTestHarness {
    pub dataspace: DataspaceRegistry,
    shutdown_tx: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), SupervisorError>>,
}

impl ServerTestHarness {
    pub async fn start(id: &str, configure: impl FnOnce(&mut Supervisor)) -> Self {
        let (capture, dataspace_rx) = DataspaceCapture::new();
        let mut supervisor = Supervisor::new(id).expect("test supervisor name should be valid");
        configure(&mut supervisor);
        supervisor.add_worker(capture);

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(async move { supervisor.run_with_shutdown(shutdown_rx).await });
        let dataspace = timeout(TEST_TIMEOUT, dataspace_rx)
            .await
            .expect("should capture the supervisor dataspace")
            .expect("dataspace capture worker should send the registry");

        Self {
            dataspace,
            shutdown_tx: Some(shutdown_tx),
            task,
        }
    }

    pub async fn bound_address(&self, id: &str) -> std::net::SocketAddr {
        let mut subscription = self
            .dataspace
            .subscribe::<BoundServerAddress>(IdentifierFilter::exact(id));

        match timeout(TEST_TIMEOUT, subscription.recv()).await {
            Ok(Some(DataspaceUpdate::Asserted(_, BoundServerAddress(address)))) => address,
            update => panic!("expected a bound address assertion for '{id}', got {update:?}"),
        }
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }

        (&mut self.task)
            .await
            .expect("test supervisor task should not panic")
            .expect("test supervisor should stop cleanly");
    }
}

impl Drop for ServerTestHarness {
    fn drop(&mut self) {
        self.shutdown_tx.take();
    }
}

struct DataspaceCapture {
    dataspace_tx: Mutex<Option<oneshot::Sender<DataspaceRegistry>>>,
}

impl DataspaceCapture {
    fn new() -> (Self, oneshot::Receiver<DataspaceRegistry>) {
        let (dataspace_tx, dataspace_rx) = oneshot::channel();
        (
            Self {
                dataspace_tx: Mutex::new(Some(dataspace_tx)),
            },
            dataspace_rx,
        )
    }
}

#[async_trait]
impl Supervisable for DataspaceCapture {
    fn name(&self) -> &str {
        "dataspace_capture"
    }

    async fn initialize(&self, process_shutdown: ShutdownHandle) -> Result<SupervisorFuture, InitializationError> {
        let dataspace = DataspaceRegistry::try_current().ok_or_else(|| generic_error!("Dataspace not available."))?;
        let dataspace_tx = self
            .dataspace_tx
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| generic_error!("DataspaceCapture can only be initialized once."))?;
        let _ = dataspace_tx.send(dataspace);

        Ok(Box::pin(async move {
            process_shutdown.await;
            Ok(())
        }))
    }
}
