//! Thin facade over the Antithesis SDK.
//!
//! This crate owns the single `antithesis` feature for the project. It is no-op
//! unless specifically enabled. Each macro forwards to the SDK macro. These
//! macros convert a trailing map into JSON for consumption by the underlying
//! SDK.
//!
//! # Use Guidance
//!
//! A more precise assertion gives Antithesis better exploration. Prefer the
//! numeric macros like `always_gt!(x, y, ...)` over `always!(x > y, ...)`.

#![deny(missing_docs)]

/// Hidden SDK handle. The macros forward through this path so call sites never
/// name `antithesis_sdk`.
#[cfg(feature = "antithesis")]
#[doc(hidden)]
pub use antithesis_sdk as __sdk;
/// Hidden `serde_json::json` handle. The macros wrap detail maps through this path so call sites need no `serde_json`
/// dependency of their own.
#[cfg(feature = "antithesis")]
#[doc(hidden)]
pub use serde_json::json as __json;

/// Initializes the Antithesis SDK and its assertion catalog. No-op without the
/// `antithesis` feature.
#[cfg(feature = "antithesis")]
pub fn init() {
    antithesis_sdk::antithesis_init();
}

/// Initializes the Antithesis SDK and its assertion catalog. No-op without the
/// `antithesis` feature.
#[cfg(not(feature = "antithesis"))]
pub fn init() {}

// Feature on. Every macro forwards to the live SDK macro at the call site.
#[cfg(feature = "antithesis")]
mod enabled {
    /// Asserts `condition` holds every time this line runs, and that the line runs at least once.
    #[macro_export]
    macro_rules! always {
        ($condition:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_always!($condition, $message, &$crate::__json!({ $($details)* }))
        };
        ($condition:expr, $message:literal) => {
            $crate::__sdk::assert_always!($condition, $message)
        };
    }

    /// Asserts `condition` holds every time this line runs. Passes even if the line never runs.
    #[macro_export]
    macro_rules! always_or_unreachable {
        ($condition:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_always_or_unreachable!($condition, $message, &$crate::__json!({ $($details)* }))
        };
        ($condition:expr, $message:literal) => {
            $crate::__sdk::assert_always_or_unreachable!($condition, $message)
        };
    }

    /// Asserts `condition` holds at least once across all runs of this line.
    #[macro_export]
    macro_rules! sometimes {
        ($condition:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_sometimes!($condition, $message, &$crate::__json!({ $($details)* }))
        };
        ($condition:expr, $message:literal) => {
            $crate::__sdk::assert_sometimes!($condition, $message)
        };
    }

    /// Asserts this line is reached at least once.
    #[macro_export]
    macro_rules! reachable {
        ($message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_reachable!($message, &$crate::__json!({ $($details)* }))
        };
        ($message:literal) => {
            $crate::__sdk::assert_reachable!($message)
        };
    }

    /// Asserts this line is never reached.
    #[macro_export]
    macro_rules! unreachable {
        ($message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_unreachable!($message, &$crate::__json!({ $($details)* }))
        };
        ($message:literal) => {
            $crate::__sdk::assert_unreachable!($message)
        };
    }

    /// Asserts `left > right` always, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! always_gt {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_always_greater_than!($left, $right, $message, &$crate::__json!({ $($details)* }))
        };
        ($left:expr, $right:expr, $message:literal) => {
            $crate::__sdk::assert_always_greater_than!($left, $right, $message)
        };
    }

    /// Asserts `left >= right` always, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! always_ge {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_always_greater_than_or_equal_to!(
                $left, $right, $message, &$crate::__json!({ $($details)* })
            )
        };
        ($left:expr, $right:expr, $message:literal) => {
            $crate::__sdk::assert_always_greater_than_or_equal_to!($left, $right, $message)
        };
    }

    /// Asserts `left < right` always, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! always_lt {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_always_less_than!($left, $right, $message, &$crate::__json!({ $($details)* }))
        };
        ($left:expr, $right:expr, $message:literal) => {
            $crate::__sdk::assert_always_less_than!($left, $right, $message)
        };
    }

    /// Asserts `left <= right` always, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! always_le {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_always_less_than_or_equal_to!(
                $left, $right, $message, &$crate::__json!({ $($details)* })
            )
        };
        ($left:expr, $right:expr, $message:literal) => {
            $crate::__sdk::assert_always_less_than_or_equal_to!($left, $right, $message)
        };
    }

    /// Asserts `left > right` at least once, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! sometimes_gt {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_sometimes_greater_than!($left, $right, $message, &$crate::__json!({ $($details)* }))
        };
        ($left:expr, $right:expr, $message:literal) => {
            $crate::__sdk::assert_sometimes_greater_than!($left, $right, $message)
        };
    }

    /// Asserts `left >= right` at least once, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! sometimes_ge {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_sometimes_greater_than_or_equal_to!(
                $left, $right, $message, &$crate::__json!({ $($details)* })
            )
        };
        ($left:expr, $right:expr, $message:literal) => {
            $crate::__sdk::assert_sometimes_greater_than_or_equal_to!($left, $right, $message)
        };
    }

    /// Asserts `left < right` at least once, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! sometimes_lt {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_sometimes_less_than!($left, $right, $message, &$crate::__json!({ $($details)* }))
        };
        ($left:expr, $right:expr, $message:literal) => {
            $crate::__sdk::assert_sometimes_less_than!($left, $right, $message)
        };
    }

    /// Asserts `left <= right` at least once, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! sometimes_le {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_sometimes_less_than_or_equal_to!(
                $left, $right, $message, &$crate::__json!({ $($details)* })
            )
        };
        ($left:expr, $right:expr, $message:literal) => {
            $crate::__sdk::assert_sometimes_less_than_or_equal_to!($left, $right, $message)
        };
    }

    /// Asserts at least one of the named conditions always holds, with per-name guidance.
    #[macro_export]
    macro_rules! always_some {
        ({ $($conditions:tt)* }, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_always_some!({ $($conditions)* }, $message, &$crate::__json!({ $($details)* }))
        };
        ({ $($conditions:tt)* }, $message:literal) => {
            $crate::__sdk::assert_always_some!({ $($conditions)* }, $message)
        };
    }

    /// Asserts every named condition holds at least once, with per-name guidance.
    #[macro_export]
    macro_rules! sometimes_all {
        ({ $($conditions:tt)* }, $message:literal, { $($details:tt)* }) => {
            $crate::__sdk::assert_sometimes_all!({ $($conditions)* }, $message, &$crate::__json!({ $($details)* }))
        };
        ({ $($conditions:tt)* }, $message:literal) => {
            $crate::__sdk::assert_sometimes_all!({ $($conditions)* }, $message)
        };
    }
}

// Feature off. Every macro is a no-op that elides its arguments unevaluated.
#[cfg(not(feature = "antithesis"))]
mod disabled {
    /// Asserts `condition` holds every time this line runs, and that the line runs at least once.
    #[macro_export]
    macro_rules! always {
        ($condition:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($condition:expr, $message:literal) => {{}};
    }

    /// Asserts `condition` holds every time this line runs. Passes even if the line never runs.
    #[macro_export]
    macro_rules! always_or_unreachable {
        ($condition:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($condition:expr, $message:literal) => {{}};
    }

    /// Asserts `condition` holds at least once across all runs of this line.
    #[macro_export]
    macro_rules! sometimes {
        ($condition:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($condition:expr, $message:literal) => {{}};
    }

    /// Asserts this line is reached at least once.
    #[macro_export]
    macro_rules! reachable {
        ($message:literal, { $($details:tt)* }) => {{}};
        ($message:literal) => {{}};
    }

    /// Asserts this line is never reached.
    #[macro_export]
    macro_rules! unreachable {
        ($message:literal, { $($details:tt)* }) => {{}};
        ($message:literal) => {{}};
    }

    /// Asserts `left > right` always, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! always_gt {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($left:expr, $right:expr, $message:literal) => {{}};
    }

    /// Asserts `left >= right` always, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! always_ge {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($left:expr, $right:expr, $message:literal) => {{}};
    }

    /// Asserts `left < right` always, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! always_lt {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($left:expr, $right:expr, $message:literal) => {{}};
    }

    /// Asserts `left <= right` always, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! always_le {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($left:expr, $right:expr, $message:literal) => {{}};
    }

    /// Asserts `left > right` at least once, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! sometimes_gt {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($left:expr, $right:expr, $message:literal) => {{}};
    }

    /// Asserts `left >= right` at least once, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! sometimes_ge {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($left:expr, $right:expr, $message:literal) => {{}};
    }

    /// Asserts `left < right` at least once, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! sometimes_lt {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($left:expr, $right:expr, $message:literal) => {{}};
    }

    /// Asserts `left <= right` at least once, with numeric guidance on the two operands.
    #[macro_export]
    macro_rules! sometimes_le {
        ($left:expr, $right:expr, $message:literal, { $($details:tt)* }) => {{}};
        ($left:expr, $right:expr, $message:literal) => {{}};
    }

    /// Asserts at least one of the named conditions always holds, with per-name guidance.
    #[macro_export]
    macro_rules! always_some {
        ({ $($conditions:tt)* }, $message:literal, { $($details:tt)* }) => {{}};
        ({ $($conditions:tt)* }, $message:literal) => {{}};
    }

    /// Asserts every named condition holds at least once, with per-name guidance.
    #[macro_export]
    macro_rules! sometimes_all {
        ({ $($conditions:tt)* }, $message:literal, { $($details:tt)* }) => {{}};
        ({ $($conditions:tt)* }, $message:literal) => {{}};
    }
}
