//! A dynamic configuration provider for `figment`.
use std::sync::Arc;

use arc_swap::ArcSwap;
use figment::{
    value::{Dict, Map, Value},
    Error, Metadata, Profile, Provider as FigmentProvider,
};
use serde_json::Value as JsonValue;

/// A `figment` provider that holds a shared reference to a `serde_json::Value` that is updated by the `RemoteAgentImpl`.
#[derive(Debug, Clone)]
pub struct Provider {
    values: Arc<ArcSwap<JsonValue>>,
}

impl Provider {
    pub fn new(values: Arc<ArcSwap<JsonValue>>) -> Self {
        Self { values }
    }
}

impl FigmentProvider for Provider {
    fn metadata(&self) -> Metadata {
        Metadata::named("dynamic")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, Error> {
        let value = self.values.load();
        let figment_value: Value =
            serde_json::from_value((*value).as_ref().clone()).map_err(|e| Error::from(e.to_string()))?;

        let dictionary = figment_value.into_dict().unwrap_or_default();

        let mut map = Map::new();
        map.insert(Profile::default(), dictionary);
        Ok(map)
    }
}
