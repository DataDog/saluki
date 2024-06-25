use std::{collections::HashMap, ops::Deref as _, sync::Arc};

use ddsketch_agent::DDSketch;
use serde::{Deserialize, Serialize};

pub mod query;

#[derive(Clone, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct MetricContextInner {
    name: String,
    tags: Vec<String>,
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct MetricContext {
    inner: Arc<MetricContextInner>,
}

impl MetricContext {
    pub fn new(name: String, mut tags: Vec<String>) -> Self {
        tags.sort_unstable();

        Self {
            inner: Arc::new(MetricContextInner { name, tags }),
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn tags(&self) -> &[String] {
        &self.inner.tags
    }
}

#[derive(Clone, Copy, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ContextToken(usize);

#[derive(Clone, Deserialize, Serialize)]
pub struct MetricUpdate<T> {
    #[serde(rename = "ts")]
    timestamp: i64,

    #[serde(rename = "i", default, skip_serializing_if = "Option::is_none")]
    interval: Option<i64>,

    #[serde(rename = "v")]
    value: T,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct MetricStore {
    /// A store of all resolved contexts.
    contexts: ContextStore,

    /// A map of all point updates for each context.
    points: HashMap<ContextToken, Vec<MetricUpdate<f64>>>,

    /// A map of all distribution updates for each context.
    distributions: HashMap<ContextToken, Vec<MetricUpdate<DDSketch>>>,
}

impl MetricStore {
    pub fn context_store(&self) -> &ContextStore {
        &self.contexts
    }

    pub fn context_store_mut(&mut self) -> &mut ContextStore {
        &mut self.contexts
    }

    pub fn add_point(&mut self, context: ContextToken, timestamp: i64, interval: i64, value: f64) {
        let points = self.points.entry(context).or_default();
        points.push(MetricUpdate { timestamp, interval: Some(interval), value });
    }

    pub fn add_distribution(&mut self, context: ContextToken, timestamp: i64, value: DDSketch) {
        let distributions = self.distributions.entry(context).or_default();
        distributions.push(MetricUpdate { timestamp, interval: None, value });
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(from = "DenormalizedContextStore", into = "DenormalizedContextStore")]
pub struct ContextStore {
    contexts_token_map: HashMap<MetricContext, ContextToken>,
    token_contexts_map: Vec<MetricContext>,
}

impl ContextStore {
    pub fn resolve_context(&mut self, name: String, tags: Vec<String>) -> ContextToken {
        let context = MetricContext::new(name, tags);

        if let Some(token) = self.contexts_token_map.get(&context) {
            *token
        } else {
            let token = ContextToken(self.contexts_token_map.len());
            self.contexts_token_map.insert(context.clone(), token);
            self.token_contexts_map.push(context);

            token
        }
    }

    pub fn contexts(&self) -> &[MetricContext] {
        &self.token_contexts_map
    }
}

#[derive(Deserialize, Serialize)]
struct DenormalizedContextStore {
    token_contexts_map: Vec<MetricContextInner>,
}

impl From<ContextStore> for DenormalizedContextStore {
    fn from(context_store: ContextStore) -> Self {
        let token_contexts_map = context_store
            .token_contexts_map
            .iter()
            .map(|context| context.inner.deref().clone())
            .collect();

        Self { token_contexts_map }
    }
}

impl From<DenormalizedContextStore> for ContextStore {
    fn from(denormalized_context_store: DenormalizedContextStore) -> Self {
        let mut token_contexts_map = Vec::new();
        let mut contexts_token_map = HashMap::new();

        for entry in denormalized_context_store.token_contexts_map {
            let context = MetricContext { inner: Arc::new(entry) };
            let token = ContextToken(token_contexts_map.len());

            token_contexts_map.push(context.clone());
            contexts_token_map.insert(context, token);
        }

        Self {
            contexts_token_map,
            token_contexts_map,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub struct DataStore {
    metrics: MetricStore,
}

impl DataStore {
    pub fn new() -> Self {
        Self {
            metrics: MetricStore::default(),
        }
    }

    pub fn metrics(&self) -> &MetricStore {
        &self.metrics
    }

    pub fn metrics_mut(&mut self) -> &mut MetricStore {
        &mut self.metrics
    }
}
