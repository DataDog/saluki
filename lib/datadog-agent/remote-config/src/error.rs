use std::fmt;

/// A result produced by the remote configuration client.
pub type Result<T> = std::result::Result<T, Error>;

/// An error produced by the remote configuration client.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The product already has a live subscription on this client.
    ///
    /// A product carries one apply status per configuration, so only one subscriber may decode it. To drive several
    /// consumers from one product, subscribe once where those consumers are wired together and give each of them a
    /// clone of that [`Subscription`](crate::Subscription): every clone observes the latest snapshot, and the product
    /// is still decoded once per snapshot.
    ///
    /// The client does not return the existing subscription, because it cannot know that the type the second caller
    /// asks for is the type the first caller subscribed with. This error therefore means the call sites need
    /// restructuring so that one of them owns the subscription and passes clones to the others, rather than that the
    /// call should be retried.
    AlreadySubscribed {
        /// The name of the product that is already subscribed, such as `APM_SEMANTIC_CORE_DD`.
        product: String,
    },

    /// A field of [`RcClientConfiguration`](crate::RcClientConfiguration) is out of range.
    InvalidSettings {
        /// Which field is out of range, its bound, and the value it was given.
        message: String,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadySubscribed { product } => write!(f, "Product {product} is already subscribed on this client."),
            Self::InvalidSettings { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// Converts a subscriber's decoding error into a rejection message for the Agent.
///
/// The protocol carries an apply error as a string, not an error category. Choose a message that identifies the
/// problem without including secrets or raw configuration payloads. A [`decode`](crate::ProductDecoder::decode) error
/// rejects that configuration alone; a [`build`](crate::ProductDecoder::build) error rejects every configuration that
/// decoded successfully.
pub trait ApplyError {
    /// Describes this error as a rejection the client can report to the Agent.
    fn apply_error(&self) -> String;
}

impl ApplyError for String {
    fn apply_error(&self) -> String {
        self.clone()
    }
}
