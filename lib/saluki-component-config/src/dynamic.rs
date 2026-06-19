//! The `DynamicConfig<T>` fixed-or-live runtime configuration handle.
//!
//! A component that can never react to runtime configuration updates takes its native config slice
//! by value (`T`). A component that is dynamic-capable takes a [`DynamicConfig<T>`] instead. The
//! handle type therefore signals "this component can react to updates" at the API level, while
//! whether the handle is currently [`DynamicConfig::Fixed`] or [`DynamicConfig::Live`] is a
//! deployment detail hidden from the component.

use std::future::pending;

use tokio::sync::watch;

/// A fixed-or-live handle to a component's native configuration slice.
///
/// `DynamicConfig<T>` wraps a configuration value of type `T`. It is either:
///
/// - [`DynamicConfig::Fixed`]: a value that never changes for the lifetime of the process. A handle
///   in this state behaves as a constant.
/// - [`DynamicConfig::Live`]: a value backed by a [`watch::Receiver<T>`] that the configuration
///   system updates when a new, valid configuration is translated. The `initial` field holds the
///   value at construction time so a caller can read a value even before the first update arrives.
///
/// The enum deliberately does not implement `PartialEq` or `Eq`. Equality on a live handle would
/// have surprising semantics (two handles watching the same channel are not the same value, and the
/// watched value changes underneath the handle), so the type forces callers to compare the values
/// they read via [`current`](DynamicConfig::current) instead.
///
/// A component consumes a `DynamicConfig<T>` in one of two explicit ways:
///
/// - Reactive: `loop { handle.changed().await; /* rebuild local state from handle.current() */ }`.
/// - Latest-read: `let cfg = handle.current();` when building a request or making a decision.
pub enum DynamicConfig<T> {
    /// A configuration value that never changes.
    ///
    /// A handle in this state is produced for components that are dynamic-capable by type but are
    /// not wired to a live update channel in the current deployment.
    Fixed(T),

    /// A configuration value backed by a live update channel.
    ///
    /// The configuration system holds the corresponding [`watch::Sender<T>`] and publishes a new
    /// value on each accepted runtime configuration update.
    Live {
        /// The value at the time the handle was constructed.
        ///
        /// This is retained so the latest value can always be read even if the underlying channel
        /// has been closed (for example, if the sender was dropped), in which case
        /// [`current`](DynamicConfig::current) falls back to this value.
        initial: T,

        /// The receiver that observes configuration updates published by the configuration system.
        rx: watch::Receiver<T>,
    },
}

impl<T: Clone> Clone for DynamicConfig<T> {
    /// Returns a handle observing the same configuration source.
    ///
    /// A [`Fixed`](DynamicConfig::Fixed) handle clones its wrapped value. A
    /// [`Live`](DynamicConfig::Live) handle clones the retained `initial` value and the underlying
    /// [`watch::Receiver`], so the clone observes the same channel: both handles see the same
    /// published updates.
    fn clone(&self) -> Self {
        match self {
            DynamicConfig::Fixed(value) => DynamicConfig::Fixed(value.clone()),
            DynamicConfig::Live { initial, rx } => DynamicConfig::Live {
                initial: initial.clone(),
                rx: rx.clone(),
            },
        }
    }
}

impl<T> DynamicConfig<T> {
    /// Creates a fixed handle wrapping the given value.
    ///
    /// The returned handle never changes: [`current`](DynamicConfig::current) always returns a clone
    /// of `value` and [`changed`](DynamicConfig::changed) never resolves.
    pub fn fixed(value: T) -> Self {
        DynamicConfig::Fixed(value)
    }

    /// Creates a live handle from an initial value and an update receiver.
    ///
    /// The initial value is read until the first update arrives on `rx`; thereafter
    /// [`current`](DynamicConfig::current) reflects the latest value observed on the channel.
    pub fn live(initial: T, rx: watch::Receiver<T>) -> Self {
        DynamicConfig::Live { initial, rx }
    }

    /// Returns `true` if this handle is live (backed by an update channel).
    ///
    /// This is a deployment detail; components should not branch on it for behavior, but it is
    /// useful for diagnostics and tests.
    pub fn is_live(&self) -> bool {
        matches!(self, DynamicConfig::Live { .. })
    }
}

impl<T: Clone> DynamicConfig<T> {
    /// Returns a clone of the latest configuration value.
    ///
    /// For a [`Fixed`](DynamicConfig::Fixed) handle this clones the wrapped value. For a
    /// [`Live`](DynamicConfig::Live) handle this clones the most recent value seen on the watch
    /// channel; if the sender has been dropped, it falls back to cloning the `initial` value.
    ///
    /// The method is named `current` rather than `borrow` to make the clone cost explicit at the
    /// call site: every call allocates a fresh `T`.
    pub fn current(&self) -> T {
        match self {
            DynamicConfig::Fixed(value) => value.clone(),
            // `borrow()` returns the most recently sent value, or the initial channel value if none
            // has been sent. It stays readable even after the sender is dropped, so the retained
            // `initial` field is a structural record of the construction-time value rather than a
            // required fallback here.
            DynamicConfig::Live { rx, .. } => rx.borrow().clone(),
        }
    }

    /// Resolves when the underlying configuration value changes.
    ///
    /// For a [`Live`](DynamicConfig::Live) handle this awaits the next update on the watch channel
    /// and resolves once a new value is available (or the channel is closed). For a
    /// [`Fixed`](DynamicConfig::Fixed) handle this never resolves, so a reactive loop that awaits
    /// `changed` on a fixed handle simply parks forever and never rebuilds state.
    pub async fn changed(&mut self) {
        match self {
            DynamicConfig::Fixed(_) => pending().await,
            DynamicConfig::Live { rx, .. } => {
                // A closed channel resolves immediately with an error; treat it as "no further
                // changes" by parking so callers do not spin on a dead channel.
                if rx.changed().await.is_err() {
                    pending().await
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::watch;
    use tokio::time::timeout;

    use super::DynamicConfig;

    #[tokio::test]
    async fn fixed_current_returns_value() {
        let handle = DynamicConfig::fixed(7u32);
        assert_eq!(handle.current(), 7);
        assert!(!handle.is_live());
    }

    #[tokio::test]
    async fn fixed_changed_never_resolves() {
        let mut handle = DynamicConfig::fixed(7u32);
        let result = timeout(Duration::from_millis(20), handle.changed()).await;
        assert!(result.is_err(), "changed() on a Fixed handle must never resolve");
    }

    #[tokio::test]
    async fn live_current_reflects_updates() {
        let (tx, rx) = watch::channel(1u32);
        let mut handle = DynamicConfig::live(1u32, rx);
        assert!(handle.is_live());
        assert_eq!(handle.current(), 1);

        tx.send(2).expect("receiver is alive");
        handle.changed().await;
        assert_eq!(handle.current(), 2);
    }

    #[tokio::test]
    async fn live_changed_parks_when_channel_closed() {
        let (tx, rx) = watch::channel(1u32);
        let mut handle = DynamicConfig::live(1u32, rx);
        drop(tx);

        // First `changed` observes the closed channel and parks rather than spinning.
        let result = timeout(Duration::from_millis(20), handle.changed()).await;
        assert!(result.is_err(), "changed() on a closed channel must park, not spin");
    }
}
