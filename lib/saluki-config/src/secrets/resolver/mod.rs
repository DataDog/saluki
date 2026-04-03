use std::collections::HashMap;

use super::{Error, KeyPath};

mod external;
pub use self::external::{ExternalProcessResolver, ExternalProcessResolverConfiguration};

pub trait Resolver {
    /// Resolves the given secrets.
    ///
    /// The returned map contains the original key paths mapped to the secret's resolved value.
    ///
    /// # Errors
    ///
    /// If there is an issue while resolving the secrets, an error will be returned.
    async fn resolve(&self, secrets: HashMap<KeyPath, String>) -> Result<HashMap<KeyPath, String>, Error>;
}
