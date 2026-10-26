mod addr;
pub use self::addr::{
    BoundListenAddress, ConnectionAddress, GrpcTargetAddress, ListenAddress, ProcessCredentials,
    ProcessCredentialsError, ProcessIdentity,
};

pub mod client;
pub mod dns;
pub mod listener;
pub mod server;

mod resource;
pub use self::resource::{ConnectionOrientedSocketSpecification, SocketSpecification};

mod stream;
pub use self::stream::{Connection, Stream};

#[cfg(unix)]
pub mod unix;

pub mod util;
