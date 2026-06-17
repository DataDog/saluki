use tokio::sync::watch;

/// Component-scoped configuration that is either fixed for the process lifetime or backed by a live watch channel.
#[derive(Debug)]
pub enum ScopedConfig<T> {
    /// A fixed configuration value.
    Fixed(T),
    /// A live configuration value with its latest accepted update.
    Live { initial: T, rx: watch::Receiver<T> },
}

impl<T: Clone> ScopedConfig<T> {
    /// Returns the current value by cloning it out of the handle.
    pub fn current(&self) -> T {
        match self {
            Self::Fixed(value) => value.clone(),
            Self::Live { rx, .. } => rx.borrow().clone(),
        }
    }

    /// Waits until the value changes.
    ///
    /// Fixed handles never complete.
    pub async fn changed(&mut self) {
        match self {
            Self::Fixed(_) => std::future::pending().await,
            Self::Live { rx, .. } => {
                let _ = rx.changed().await;
            }
        }
    }

    /// Creates a fixed handle.
    pub fn fixed(value: T) -> Self {
        Self::Fixed(value)
    }

    /// Creates a live handle and its sending side.
    pub fn live(initial: T) -> (Self, watch::Sender<T>) {
        let (tx, rx) = watch::channel(initial.clone());
        (Self::Live { initial, rx }, tx)
    }
}

impl<T: Clone> Clone for ScopedConfig<T> {
    fn clone(&self) -> Self {
        match self {
            Self::Fixed(value) => Self::Fixed(value.clone()),
            Self::Live { initial, rx } => Self::Live {
                initial: initial.clone(),
                rx: rx.clone(),
            },
        }
    }
}
