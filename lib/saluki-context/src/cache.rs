use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
    thread,
    time::Duration,
};

use crossbeam_queue::ArrayQueue;
use saluki_time::get_unix_timestamp_coarse;
use tracing::trace;

use crate::{hash::NoopU64Hasher, Context};

#[derive(Debug)]
struct Inner {
    name: String,
    contexts: RwLock<HashMap<u64, Context, NoopU64Hasher>>,
    contexts_last_seen: Mutex<HashMap<u64, u64, NoopU64Hasher>>,
    contexts_touched_pending: ArrayQueue<u64>,
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
            contexts_touched_pending: ArrayQueue::new(1024),
        });

        let bg_inner = Arc::clone(&inner);
        thread::spawn(move || run_background_expiration(bg_inner));

        Self { inner }
    }

    pub fn get_or_insert<F>(&self, id: u64, f: F) -> Option<Context>
    where
        F: FnOnce() -> Option<Context>,
    {
        // First, we try and find the cached context, and if it doesn't exist, we'll create it via `f`.
        let context = {
            let contexts_read = self.inner.contexts.read().unwrap();
            match contexts_read.get(&id) {
                Some(context) => context.clone(),
                None => {
                    drop(contexts_read);

                    let context = f()?;
                    let context_to_insert = context.clone();
                    let mut contexts_write = self.inner.contexts.write().unwrap();
                    contexts_write.insert(id, context_to_insert);

                    context
                }
            }
        };

        // Now we update the recency state for this context.
        self.track_context_touched(id);

        Some(context)
    }

    pub fn len(&self) -> usize {
        self.inner.contexts.read().unwrap().len()
    }

    fn track_context_touched(&self, id: u64) {
        // Try to push the context ID to the pending queue. If the queue is currently full, we'll get back the oldest
        // item from the queue, which we have now just replaced, and we'll incrementally update the last seen time.
        if let Some(old_pending_id) = self.inner.contexts_touched_pending.force_push(id) {
            self.with_last_seen_lock(|contexts_last_seen| {
                let last_seen = contexts_last_seen.entry(old_pending_id).or_default();
                *last_seen = get_unix_timestamp_coarse();
            });
        }
    }

    fn with_last_seen_lock<F>(&self, f: F)
    where
        F: FnOnce(&mut HashMap<u64, u64, NoopU64Hasher>),
    {
        let mut contexts_last_seen = self.inner.contexts_last_seen.lock().unwrap();
        f(&mut contexts_last_seen);
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

        let mut contexts_updated = 0;
        let mut last_seen_write = bg_inner.contexts_last_seen.lock().unwrap();
        while let Some(pending_context_id) = bg_inner.contexts_touched_pending.pop() {
            let last_seen = last_seen_write.entry(pending_context_id).or_default();
            *last_seen = current_time;
            contexts_updated += 1;
        }

        trace!(
            resolver_id = bg_inner.name,
            contexts_updated,
            "Updated last seen for contexts."
        );

        // Now iterate through our last seen map and figure out which contexts are eligible for expiration.
        for (context_id, last_seen) in last_seen_write.iter() {
            if current_time - last_seen > CONTEXT_EXPIRATION_IDLE_SECONDS {
                contexts_to_expire.push(*context_id);
            }
        }

        if contexts_to_expire.is_empty() {
            continue;
        }

        // Finally, expire the contexts.
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
