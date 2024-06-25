use crate::{DataStore, MetricContext, MetricUpdate};

pub struct DataStoreQuerier {
    store: DataStore,
}

impl DataStoreQuerier {
    pub fn metrics(&self) -> MetricStoreQuerier<'_> {
        MetricStoreQuerier::from(&self.store)
    }
}

impl From<DataStore> for DataStoreQuerier {
    fn from(store: DataStore) -> Self {
        Self { store }
    }
}

pub struct MetricStoreQuerier<'a> {
    store: &'a DataStore,
}

impl<'a> MetricStoreQuerier<'a> {
    pub fn contexts(&self) -> &[MetricContext] {
        self.store.metrics.contexts.contexts()
    }

    pub fn contexts_by_name(&self, name: &'a str) -> impl Iterator<Item = &'a MetricContext> {
        self.store
            .metrics
            .contexts
            .contexts()
            .iter()
            .filter(move |context| context.name() == name)
    }

    pub fn points_for_context(&self, context: &MetricContext) -> impl Iterator<Item = &MetricUpdate<f64>> {
        let context_token = self
            .store
            .metrics
            .contexts
            .contexts_token_map
            .get(context)
            .expect("should never get metric context that doesn't match data store");
        match self.store.metrics.points.get(context_token) {
            Some(points) => points.as_slice().iter(),
            None => [].iter(),
        }
    }

	pub fn get_point_total_for_context(&self, context: &MetricContext) -> f64 {
		self.points_for_context(context).map(|point| point.value * point.interval.unwrap_or(1) as f64).sum()
	}
}

impl<'a> From<&'a DataStore> for MetricStoreQuerier<'a> {
    fn from(store: &'a DataStore) -> Self {
        Self { store }
    }
}
