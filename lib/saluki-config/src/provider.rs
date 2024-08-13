use std::path::Path;

use figment::{
    providers::Data,
    value::{Dict, Map},
    Error, Metadata, Profile, Provider,
};

pub struct ResolvedProvider {
    data: Map<Profile, Dict>,
    metadata: Metadata,
}

impl ResolvedProvider {
    pub fn from_yaml<P>(path: P) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        let file_data = std::fs::read_to_string(path.as_ref()).map_err(|e| e.to_string())?;

        let provider = Data::<figment::providers::Yaml>::string(&file_data);
        let data = provider.data()?;

        Ok(Self {
            data,
            metadata: Metadata::from("YAML file", path.as_ref()),
        })
    }

    pub fn from_json<P>(path: P) -> Result<Self, Error>
    where
        P: AsRef<Path>,
    {
        let file_data = std::fs::read_to_string(path.as_ref()).map_err(|e| e.to_string())?;

        let provider = Data::<figment::providers::Json>::string(&file_data);
        let data = provider.data()?;

        Ok(Self {
            data,
            metadata: Metadata::from("JSON file", path.as_ref()),
        })
    }
}

impl Provider for ResolvedProvider {
    fn metadata(&self) -> Metadata {
        self.metadata.clone()
    }

    fn data(&self) -> Result<Map<Profile, Dict>, Error> {
        Ok(self.data.clone())
    }
}
