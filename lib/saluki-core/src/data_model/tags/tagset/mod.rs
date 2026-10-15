pub(crate) mod frozen;

mod owned;
pub use self::owned::TagSet;

mod shared;
pub use self::shared::{SharedTagSet, SharedTagSetIterator};
