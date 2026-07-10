#![cfg(target_endian = "big")]

use std::{num::NonZeroUsize, sync::Arc};

use protobuf::Chars;
use stringtheory::{
    interning::{GenericMapInterner, Interner},
    CheapMetaString, MetaString,
};

#[test]
fn meta_string_preserves_public_api_on_big_endian() {
    let empty = MetaString::empty();
    assert!(empty.is_empty());
    assert_eq!(&*empty, "");

    let static_value = MetaString::from_static("static-value");
    assert_eq!(&*static_value, "static-value");
    assert!(static_value.is_cheaply_cloneable());

    let short = MetaString::try_inline("short").expect("short strings should fit the inline API contract");
    assert_eq!(&*short, "short");
    assert!(short.is_cheaply_cloneable());

    let long_input = "a string long enough to bypass inline storage";
    assert!(MetaString::try_inline(long_input).is_none());

    let interner = GenericMapInterner::new(NonZeroUsize::new(1024).unwrap());
    let interned = MetaString::from_interner(long_input, &interner);
    assert_eq!(&*interned, long_input);
    assert_eq!(interner.len(), 1);

    let owned = MetaString::from(String::from("owned-value"));
    assert_eq!(owned.clone().into_owned(), "owned-value");
    assert!(!owned.is_cheaply_cloneable());

    let shared = MetaString::from(Arc::<str>::from("shared-value"));
    assert_eq!(&*shared, "shared-value");
    assert_eq!(shared.try_cheap_clone().as_deref(), Some("shared-value"));

    let mut values = vec![
        MetaString::from("gamma"),
        MetaString::from("alpha"),
        MetaString::from("beta"),
    ];
    values.sort();
    assert_eq!(
        values.iter().map(|value| &**value).collect::<Vec<_>>(),
        ["alpha", "beta", "gamma"]
    );

    let chars: Chars = MetaString::from("protobuf-value").into();
    let chars_ref: &str = chars.as_ref();
    assert_eq!(chars_ref, "protobuf-value");
}
