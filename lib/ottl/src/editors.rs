//! Standard OTTL editor functions.
//!
//! Editors are callback functions invoked as top-level OTTL statements (for example, `set(target, value)`).
//! This module provides the set of editors defined by the
//! [OpenTelemetry Transformation Language specification][ottl-funcs] so that integrators can
//! bootstrap a [`CallbackMap`] without re-implementing common logic.
//!
//! [ottl-funcs]: https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/pkg/ottl/ottlfuncs
//!
//! # Example
//!
//! ```ignore
//! // Start with the standard editors…
//! let mut editors = ottl::editors::standard();
//!
//! // …and extend with project-specific editors if needed.
//! editors.insert("my_editor".to_string(), Arc::new(|args: &mut dyn ottl::Args| {
//!     todo!()
//! }));
//! ```

use std::sync::Arc;

use crate::{Args, CallbackMap, Value};

/// Returns a [`CallbackMap`] pre-populated with the standard OTTL editor functions.
///
/// Currently includes:
///
/// | Editor | Signature | Description |
/// |--------|-----------|-------------|
/// | `set`  | `set(target, value)` | Sets `target` to `value`. |
pub fn standard() -> CallbackMap {
    let mut map = CallbackMap::new();

    map.insert(
        "set".to_string(),
        Arc::new(|args: &mut dyn Args| {
            if args.len() != 2 {
                return Err(format!("set() requires exactly 2 arguments, got {}", args.len()).into());
            }
            let value = args.get(1)?;
            args.set(0, &value)?;
            Ok(Value::Nil)
        }),
    );

    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_contains_set() {
        let editors = standard();
        assert!(editors.contains_key("set"), "standard editors must include `set`");
    }
}
