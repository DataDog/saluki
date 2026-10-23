#[cfg(unix)]
use std::path::Path;
use std::{io::ErrorKind, net::SocketAddr, time::Duration};

use saluki_core::{
    components::test_util::TestComponentSupervisor,
    runtime::{
        state::{DataspaceUpdate, Identifier, IdentifierFilter},
        Supervisor,
    },
};
#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::{
    net::TcpStream,
    time::{sleep, timeout},
};

use crate::net::addr::BoundListenAddress;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

pub async fn connect_tcp(addr: SocketAddr) -> TcpStream {
    timeout(TEST_TIMEOUT, async {
        loop {
            match TcpStream::connect(addr).await {
                Ok(stream) => return stream,
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
                    sleep(Duration::from_millis(5)).await;
                }
                Err(e) => panic!("should connect to TCP listener: {e}"),
            }
        }
    })
    .await
    .expect("should connect to TCP listener before timeout")
}

#[cfg(unix)]
pub async fn connect_unix(path: &Path) -> UnixStream {
    timeout(TEST_TIMEOUT, async {
        loop {
            match UnixStream::connect(path).await {
                Ok(stream) => return stream,
                Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused) => {
                    sleep(Duration::from_millis(5)).await;
                }
                Err(e) => panic!("should connect to Unix listener: {e}"),
            }
        }
    })
    .await
    .expect("should connect to Unix listener before timeout")
}

pub struct ServerTestHarness {
    supervisor: TestComponentSupervisor,
    bound_address_id: Identifier,
}

impl ServerTestHarness {
    pub async fn start(id: &str, configure: impl FnOnce(&mut Supervisor, &str)) -> Self {
        let supervisor = TestComponentSupervisor::start_with(id, |supervisor| configure(supervisor, id)).await;

        Self {
            supervisor,
            bound_address_id: Identifier::from(format!("http-server-{}", id)),
        }
    }

    pub async fn bound_address(&self) -> BoundListenAddress {
        let mut subscription = self
            .supervisor
            .dataspace()
            .subscribe::<BoundListenAddress>(IdentifierFilter::exact(self.bound_address_id.clone()));

        match timeout(TEST_TIMEOUT, subscription.recv()).await {
            Ok(Some(DataspaceUpdate::Asserted(_, address))) => address,
            update => panic!(
                "expected a bound address assertion for '{:?}', got {update:?}",
                self.bound_address_id
            ),
        }
    }

    pub async fn bound_tcp_address(&self) -> SocketAddr {
        let bound_address = self.bound_address().await;
        match bound_address {
            BoundListenAddress::Tcp(addr) => addr,
            update => panic!(
                "expected a TCP address for bound address assertion for '{:?}', got {update:?}",
                self.bound_address_id
            ),
        }
    }

    pub async fn shutdown(self) {
        self.supervisor
            .shutdown()
            .await
            .expect("test supervisor should stop cleanly");
    }
}
