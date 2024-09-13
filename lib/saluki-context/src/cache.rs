use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    thread,
    time::Duration,
};

use saluki_time::get_unix_timestamp_coarse;
use tracing::trace;

use crate::{hash::NoopU64Hasher, Context};

#[derive(Debug)]
struct Inner {
    name: String,
    contexts: RwLock<HashMap<u64, Context, NoopU64Hasher>>,
    contexts_last_seen: Mutex<HashMap<u64, Context, NoopU64Hasher>>,
}

#[derive(Clone, Debug)]
pub struct ContextCache {
    inner: Arc<Inner>,
}

impl ContextCache {
    pub fn new(name: String) -> Self {
        let inner = Arc::new(Inner {
            name,
            contexts: RwLock::new(HashMap::with_hasher(NoopU64Hasher::new())),
            contexts_last_seen: Mutex::new(HashMap::with_hasher(NoopU64Hasher::new())),
        });

        let bg_inner = Arc::clone(&inner);
        thread::spawn(move || run_background_expiration(bg_inner));

        Self { inner }
    }

    pub fn get_or_insert<F>(&self, id: u64, f: F) -> Option<Context>
    where
        F: FnOnce() -> Option<Context>,
    {
        let mut is_new_context = false;

        // First, we try and find the cached context, and if it doesn't exist, we'll create it via `f`.
        let context = {
            let contexts_read = self.inner.contexts.read().unwrap();
            match contexts_read.get(&id) {
                Some(context) => context.clone(),
                None => {
                    drop(contexts_read);

                    is_new_context = true;

                    let context = f()?;
                    let context_to_insert = context.clone();
                    let mut contexts_write = self.inner.contexts.write().unwrap();
                    contexts_write.insert(id, context_to_insert);

                    context
                }
            }
        };

        // Now we update the recency state for this context.
        self.track_context_touched(id, &context, is_new_context);

        Some(context)
    }

    pub fn len(&self) -> usize {
        self.inner.contexts.read().unwrap().len()
    }

    fn track_context_touched(&self, id: u64, context: &Context, is_new_context: bool) {
        // Update the context's "last touched" time.
        context.update_last_touched();

        // If this is a new context, we'll add it to the pending queue, which the background updated will use to update
        // our tracking map.
        if is_new_context {
            let mut last_seen_write = self.inner.contexts_last_seen.lock().unwrap();
            last_seen_write.entry(id).or_insert_with(|| context.clone());
        }
    }
}

fn run_background_expiration(bg_inner: Arc<Inner>) {
    const CONTEXT_EXPIRATION_IDLE_SECONDS: u64 = 30;

    let mut contexts_to_expire = Vec::new();

    loop {
        thread::sleep(Duration::from_secs(1));

        // Get the current time, and then pop every pending context ID off the queue and update their entry.
        let current_time = get_unix_timestamp_coarse();
        trace!(resolver_id = bg_inner.name, "Running background expiration.");

        // Iterate through our last seen map and figure out which contexts are eligible for expiration.
        let mut last_seen_write = bg_inner.contexts_last_seen.lock().unwrap();
        for (context_id, context) in last_seen_write.iter() {
            if current_time - context.last_touched() > CONTEXT_EXPIRATION_IDLE_SECONDS {
                contexts_to_expire.push(*context_id);
            }
        }

        if contexts_to_expire.is_empty() {
            continue;
        }

        // Now expire the contexts.
        let mut contexts_write = bg_inner.contexts.write().unwrap();

        trace!(
            resolver_id = bg_inner.name,
            contexts_len = contexts_write.len(),
            contexts_to_expire = contexts_to_expire.len(),
            "Expiring contexts."
        );
        for context_id in contexts_to_expire.drain(..) {
            contexts_write.remove(&context_id);
            last_seen_write.remove(&context_id);
        }

        trace!(
            resolver_id = bg_inner.name,
            contexts_len = contexts_write.len(),
            "Finished expiring contexts."
        );
    }
}
