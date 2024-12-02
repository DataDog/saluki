use std::collections::HashMap;

use figment::Source;

use super::{Error, KeyPath};

mod external;
pub use self::external::{ExternalProcessResolver, ExternalProcessResolverConfiguration};

pub trait Resolver {
    /// Returns the source metadata for the resolver.
    ///
    /// This is used to indicate where the resolved secrets, if any, originate from.
    fn source(&self) -> Source;

    /// Resolves the given secrets.
    ///
    /// The returned map contains the original key paths mapped to the secret's resolved value.
    ///
    /// # Errors
    ///
    /// If there is an issue while resolving the secrets, an error will be returned.
    async fn resolve(&self, secrets: HashMap<KeyPath, String>) -> Result<HashMap<KeyPath, String>, Error>;
}
