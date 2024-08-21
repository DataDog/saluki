use figment::{value::{Dict, Map}, Error, Metadata, Profile, Provider};
use reqwest::Url;

pub struct RemoteHttpProviderBuilder {
	url: Url,
}

pub struct RemoteHttpProvider {
	data: Map<Profile, Dict>,
}

impl Provider for RemoteHttpProvider {
	fn metadata(&self) -> Metadata {
		todo!()
	}

	fn data(&self) -> Result<Map<Profile, Dict>, Error> {
		todo!()
	}
}