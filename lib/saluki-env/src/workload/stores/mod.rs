mod external_data;
pub use self::external_data::{ExternalDataStore, ExternalDataStoreResolver};

mod origin;

mod tag;
pub use self::tag::{TagStore, TagStoreQuerier};
