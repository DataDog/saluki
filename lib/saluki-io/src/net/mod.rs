mod addr;
pub use self::addr::{ConnectionAddress, GrpcTargetAddress, ListenAddress, ProcessCredentials};

pub mod client;
pub mod listener;
pub mod server;

mod stream;
pub use self::stream::{Connection, Stream};

#[cfg(unix)]
pub mod unix;

pub mod util;

mod ipc;
pub use self::ipc::{
    build_datadog_agent_client_ipc_tls_config, build_datadog_agent_ipc_https_connector,
    build_datadog_agent_server_tls_config, get_ipc_cert_file_path,
};
