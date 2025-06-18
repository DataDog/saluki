//! Generic helper types/functions for building Tonic-based gRPC clients.

use std::{path::Path, str::FromStr as _};

use saluki_error::{generic_error, GenericError};
use tonic::{
    metadata::{Ascii, MetadataValue},
    service::Interceptor,
    Request, Status,
};

/// A Tonic request interceptor that adds a bearer token to the request metadata.
#[derive(Clone)]
pub struct BearerAuthInterceptor {
    bearer_token: MetadataValue<Ascii>,
}

impl BearerAuthInterceptor {
    /// Creates a new `BearerAuthInterceptor` from the bearer token in the given file.
    ///
    /// # Errors
    ///
    /// If the file path is invalid, if the file cannot be read, or if the bearer token is not valid ASCII, an error
    /// variant will be returned.
    pub async fn from_file<P>(file_path: P) -> Result<Self, GenericError>
    where
        P: AsRef<Path>,
    {
        let file_path = file_path.as_ref();
        let raw_bearer_token = tokio::fs::read_to_string(file_path).await.map_err(|e| {
            generic_error!(
                "Failed to read bearer token from file '{}' ({}).",
                file_path.display(),
                e.kind()
            )
        })?;

        let raw_bearer_token = format!("bearer {}", raw_bearer_token);
        match MetadataValue::<Ascii>::from_str(&raw_bearer_token) {
            Ok(bearer_token) => Ok(Self { bearer_token }),
            Err(_) => Err(generic_error!(
                "Found invalid characters for bearer token in file '{}'. Bearer token can only contain visible ASCII characters (0x20 - 0x7F).",
                file_path.display(),
            )),
        }
    }
}

impl Interceptor for BearerAuthInterceptor {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request
            .metadata_mut()
            .insert("authorization", self.bearer_token.clone());
        Ok(request)
    }
}
