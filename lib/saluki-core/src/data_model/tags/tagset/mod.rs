pub(crate) mod frozen;

mod owned;
pub use self::owned::TagSet;
pub(crate) use self::owned::TagSetIter;

mod shared;
pub use self::shared::{SharedTagSet, SharedTagSetIterator};
