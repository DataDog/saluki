#![allow(dead_code)]

use std::io;

use http::{uri::PathAndQuery, HeaderValue, Method, Request, Uri};
use saluki_core::pooling::ObjectPool;
use saluki_io::{
    buf::{BytesBuffer, ChunkedBytesBuffer, ChunkedBytesBufferObjectPool, FrozenChunkedBytesBuffer},
    compression::*,
};
use snafu::{ResultExt, Snafu};
use tokio::io::AsyncWriteExt as _;
use tracing::{debug, error, trace};

const SCRATCH_BUF_CAPACITY: usize = 8192;

/// Encodes input events for a specific intake endpoint.
pub trait EndpointEncoder: std::fmt::Debug {
    /// The type of input events that this encoder can handle.
    type Input: std::fmt::Debug;

    /// The error type returned during encoding.
    type EncodeError: std::error::Error + 'static;

    /// Returns the name of the encoder.
    fn encoder_name() -> &'static str;

    /// Returns the maximum size of the compressed payload in bytes.
    fn compressed_size_limit(&self) -> usize;

    /// Returns the maximum size of the uncompressed payload in bytes.
    fn uncompressed_size_limit(&self) -> usize;

    /// Returns `true` if the given input is valid for this encoder.
    ///
    /// This method has a default implementation that always returns `true`, but can be overridden by specific encoders
    /// to perform additional validation.
    fn is_valid_input(&self, _input: &Self::Input) -> bool {
        // By default, we assume all inputs are valid.
        true
    }

    /// Returns the prefix that should be written at the start of every new payload.
    fn get_payload_prefix(&self) -> Option<&'static [u8]> {
        None
    }

    /// Returns the suffix that should be written at the end of every payload.
    fn get_payload_suffix(&self) -> Option<&'static [u8]> {
        None
    }

    /// Returns the separate to insert between inputs in the payload.
    ///
    /// Only used when more than one input has been encoded in the current payload.
    fn get_input_separator(&self) -> Option<&'static [u8]> {
        None
    }

    /// Encodes the given input and writes it to the given buffer.
    ///
    /// Implementations MUST NOT clear the buffer before writing to it, as additional data may already be present in the
    /// buffer to satisfy the encoding requirements of the endpoint, such as any configured prefix or input separator.
    ///
    /// # Errors
    ///
    /// If the input cannot otherwise be encoded for any reason, an error will be returned.
    fn encode(&self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError>;

    /// Returns the URI of the endpoint that this encoder is associated with.
    fn endpoint_uri(&self) -> Uri;

    /// Returns the HTTP method used for the endpoint.
    fn endpoint_method(&self) -> Method;

    /// Returns the content type of the payload.
    ///
    /// This should be the corresponding MIME type for the encoded form of input events.
    fn content_type(&self) -> HeaderValue;
}

// Request builder errors.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(false)))]
pub enum RequestBuilderError<E>
where
    E: EndpointEncoder,
{
    #[snafu(display(
        "uncompressed size limit is lower ({} bytes) than minimum payload size ({} bytes) ({} byte(s) for prefix, {} byte(s) for suffix)",
        uncompressed_size_limit,
        prefix_len + suffix_len + 1,
        prefix_len,
        suffix_len,
    ))]
    UncompressedSizeLimitTooLow {
        uncompressed_size_limit: usize,
        prefix_len: usize,
        suffix_len: usize,
    },
    #[snafu(display("input was invalid for request builder: {:?}'", input))]
    InvalidInput { input: E::Input },
    #[snafu(display("failed to encode/write payload: {}", source))]
    FailedToEncode { source: E::EncodeError },
    #[snafu(display(
        "request payload was too large after compressing ({} > {})",
        compressed_size_bytes,
        compressed_limit_bytes
    ))]
    PayloadTooLarge {
        compressed_size_bytes: usize,
        compressed_limit_bytes: usize,
    },
    #[snafu(display("failed to write/compress payload: {}", source))]
    Io { source: io::Error },
    #[snafu(display("error when building API endpoint/request: {}", source))]
    Http { source: http::Error },
}

impl<E> RequestBuilderError<E>
where
    E: EndpointEncoder,
{
    /// Returns `true` if the error is recoverable, allowing the request builder to continue to be used.
    pub fn is_recoverable(&self) -> bool {
        match self {
            // If the wrong input type is being sent to the wrong endpoint's request builder, that's just a flat out
            // bug, so we can't possibly recover.
            Self::InvalidInput { .. } => false,
            // I/O errors should only be getting created for compressor-related operations, and the scenarios in which
            // there are I/O errors should generally be very narrowly scoped to "the system is in a very bad state", so
            // we can't really recover from those... or perhaps _shouldn't_ try to recover from those.
            Self::Io { .. } => false,
            _ => true,
        }
    }
}

/// Generic builder for creating HTTP requests with payloads consisting of encoded and compressed input events.
pub struct RequestBuilder<E, O>
where
    E: EndpointEncoder,
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    encoder: E,
    endpoint_uri: Uri,
    buffer_pool: ChunkedBytesBufferObjectPool<O>,
    scratch_buf: Vec<u8>,
    compression_scheme: CompressionScheme,
    compressor: Compressor<ChunkedBytesBuffer<O>>,
    compression_estimator: CompressionEstimator,
    uncompressed_len: usize,
    compressed_len_limit: usize,
    uncompressed_len_limit: usize,
    max_inputs_per_payload: usize,
    encoded_inputs: Vec<E::Input>,
}

impl<E, O> RequestBuilder<E, O>
where
    E: EndpointEncoder,
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    /// Creates a new `RequestBuilder` with the given buffer pool, encoder, and compression scheme.
    ///
    /// The buffer pool will be drawn upon for holding the compressed payload, which will be compressed using the given
    /// compression scheme. The encoder will be used to encode input events as well as help construct the resulting HTTP
    /// requests.
    pub async fn new(
        encoder: E, buffer_pool: O, compression_scheme: CompressionScheme,
    ) -> Result<Self, RequestBuilderError<E>> {
        let endpoint_uri = encoder.endpoint_uri();
        let compressed_len_limit = encoder.compressed_size_limit();
        let uncompressed_len_limit = encoder.uncompressed_size_limit();

        // Make sure the uncompressed size limit is large enough to accommodate the prefix and suffix and an additional
        // byte: this is the smallest possible valid payload that could conceivably be written, and so we have to be
        // able to at least fit that.
        let prefix_len = encoder.get_payload_prefix().map_or(0, |p| p.len());
        let suffix_len = encoder.get_payload_suffix().map_or(0, |s| s.len());
        if uncompressed_len_limit < (prefix_len + suffix_len) + 1 {
            return Err(RequestBuilderError::UncompressedSizeLimitTooLow {
                uncompressed_size_limit: uncompressed_len_limit,
                prefix_len,
                suffix_len,
            });
        }

        let chunked_buffer_pool = ChunkedBytesBufferObjectPool::new(buffer_pool);
        let compressor = create_compressor(&chunked_buffer_pool, compression_scheme).await;
        Ok(Self {
            encoder,
            endpoint_uri,
            buffer_pool: chunked_buffer_pool,
            scratch_buf: Vec::with_capacity(SCRATCH_BUF_CAPACITY),
            compression_scheme,
            compressor,
            compression_estimator: CompressionEstimator::default(),
            uncompressed_len: 0,
            compressed_len_limit,
            uncompressed_len_limit,
            max_inputs_per_payload: usize::MAX,
            encoded_inputs: Vec::new(),
        })
    }

    /// Overrides the endpoint URI for the request builder.
    pub fn with_endpoint_uri_override(&mut self, endpoint_uri: &'static str) -> &mut Self {
        self.endpoint_uri = PathAndQuery::from_static(endpoint_uri).into();
        self
    }

    /// Sets the maximum number of inputs that can be encoded in a single payload.
    pub fn with_max_inputs_per_payload(&mut self, max_inputs_per_payload: usize) -> &mut Self {
        self.max_inputs_per_payload = max_inputs_per_payload;
        self
    }

    /// Configures custom (un)compressed length limits for the request builder.
    ///
    /// Used specifically for testing purposes.
    #[cfg(test)]
    fn set_custom_len_limits(&mut self, uncompressed_len_limit: usize, compressed_len_limit: usize) {
        self.uncompressed_len_limit = uncompressed_len_limit;
        self.compressed_len_limit = compressed_len_limit;
    }

    /// Returns a reference to the encoder used by the request builder.
    pub const fn encoder(&self) -> &E {
        &self.encoder
    }

    async fn prepare_for_write(&mut self) -> Result<(), io::Error> {
        // If we haven't written any inputs yet, and we have a payload prefix, we need to write that.
        if self.encoded_inputs.is_empty() {
            if let Some(prefix) = self.encoder.get_payload_prefix() {
                self.write_to_compressor(prefix).await?;
            }
        } else {
            // Similarly, if we've already written inputs, and we have an input separator, we need to write that.
            if let Some(separator) = self.encoder.get_input_separator() {
                self.write_to_compressor(separator).await?;
            }
        }

        Ok(())
    }

    async fn write_to_compressor(&mut self, buf: &[u8]) -> Result<(), io::Error> {
        self.compressor.write_all(buf).await?;
        self.compression_estimator.track_write(&self.compressor, buf.len());
        self.uncompressed_len += buf.len();

        Ok(())
    }

    /// Attempts to encode the input event and write it to the current request payload.
    ///
    /// If the input event can't be encoded due to size constraints, `Ok(Some(input))` will be returned, and the caller
    /// must call `flush` before attempting to encode the same input event again. Otherwise, `Ok(None)` is returned.
    ///
    /// # Errors
    ///
    /// If the given input is not valid for the configured encoder, or if there is an error during compression of the
    /// encoded input, an error will be returned.
    pub async fn encode(&mut self, input: E::Input) -> Result<Option<E::Input>, RequestBuilderError<E>> {
        // Check if the input is valid for this encoder.
        if !self.encoder.is_valid_input(&input) {
            return Err(RequestBuilderError::InvalidInput { input });
        }

        // Make sure we haven't hit the maximum number of inputs per payload.
        if self.encoded_inputs.len() >= self.max_inputs_per_payload {
            return Ok(Some(input));
        }

        self.prepare_for_write().await.context(Io)?;

        // Encode the input and then see if it will fit into the current request payload.
        //
        // If not, we return the original input, signaling to the caller that they need to flush the current request
        // payload before encoding additional inputs.
        self.scratch_buf.clear();

        self.encoder
            .encode(&input, &mut self.scratch_buf)
            .context(FailedToEncode)?;

        // If the input can't fit into the current request payload based on the uncompressed size limit, or isn't likely
        // to fit into the current request payload based on the estimated compressed size limit, then return it to the
        // caller: this indicates that a flush must happen before trying to encode the same input again.
        let encoded = self.scratch_buf.as_slice();
        let new_uncompressed_len = self.uncompressed_len + encoded.len();
        if new_uncompressed_len > self.uncompressed_len_limit
            || self
                .compression_estimator
                .would_write_exceed_threshold(encoded.len(), self.compressed_len_limit)
        {
            trace!(
                encoder = E::encoder_name(),
                endpoint = ?self.endpoint_uri,
                encoded_len = encoded.len(),
                uncompressed_len = self.uncompressed_len,
                estimated_compressed_len = self.compression_estimator.estimated_len(),
                "Input would exceed endpoint size limits."
            );
            return Ok(Some(input));
        }

        // Write the scratch buffer to the compressor.
        self.compressor.write_all(encoded).await.context(Io)?;
        self.compression_estimator.track_write(&self.compressor, encoded.len());
        self.uncompressed_len += encoded.len();
        self.encoded_inputs.push(input);

        trace!(
            encoder = E::encoder_name(),
            endpoint = ?self.endpoint_uri,
            encoded_len = encoded.len(),
            uncompressed_len = self.uncompressed_len,
            estimated_compressed_len = self.compression_estimator.estimated_len(),
            "Wrote encoded input to compressor."
        );

        Ok(None)
    }

    /// Flushes the current request payload.
    ///
    /// This resets the internal state and prepares the request builder for further encoding. If there is no data to
    /// flush, this method will return `Ok(None)`.
    ///
    /// This attempts to split the request payload into two smaller payloads if the original request payload is too large.
    ///
    /// # Errors
    ///
    /// If an error occurs while finalizing the compressor or creating the request, an error will be returned.
    pub async fn flush(&mut self) -> Vec<Result<(usize, Request<FrozenChunkedBytesBuffer>), RequestBuilderError<E>>> {
        if self.encoded_inputs.is_empty() {
            return vec![];
        }

        // Finalize the payload by writing the payload suffix, if one is configured.
        if let Some(suffix) = self.encoder.get_payload_suffix() {
            if let Err(e) = self.write_to_compressor(suffix).await.context(Io) {
                return vec![Err(e)];
            }
        }

        // Clear our internal state and finalize the compressor. We do it in this order so that if finalization fails,
        // somehow, the request builder is in a default state and encoding can be attempted again.
        let uncompressed_len = self.uncompressed_len;
        self.uncompressed_len = 0;

        self.compression_estimator.reset();

        let new_compressor = create_compressor(&self.buffer_pool, self.compression_scheme).await;
        let mut compressor = std::mem::replace(&mut self.compressor, new_compressor);
        if let Err(e) = compressor.flush().await.context(Io) {
            let inputs_dropped = self.clear_encoded_inputs();

            // TODO: Propagate the number of inputs dropped in the returned error itself rather than logging here.
            error!(
                encoder = E::encoder_name(),
                endpoint = ?self.endpoint_uri,
                inputs_dropped,
                "Failed to finalize compressor while building request. Inputs have been dropped."
            );

            return vec![Err(e)];
        }

        if let Err(e) = compressor.shutdown().await.context(Io) {
            let inputs_dropped = self.clear_encoded_inputs();

            // TODO: Propagate the number of inputs dropped in the returned error itself rather than logging here.
            error!(
                encoder = E::encoder_name(),
                endpoint = ?self.endpoint_uri,
                inputs_dropped,
                "Failed to finalize compressor while building request. Inputs have been dropped."
            );

            return vec![Err(e)];
        }

        let buffer = compressor.into_inner().freeze();

        let compressed_len = buffer.len();
        let compressed_limit = self.compressed_len_limit;
        if compressed_len > compressed_limit {
            // Single input is unable to be split.
            if self.encoded_inputs.len() == 1 {
                let _ = self.clear_encoded_inputs();

                return vec![Err(RequestBuilderError::PayloadTooLarge {
                    compressed_size_bytes: compressed_len,
                    compressed_limit_bytes: compressed_limit,
                })];
            }

            return self.split_request().await;
        }

        let inputs_written = self.clear_encoded_inputs();
        debug!(encoder = E::encoder_name(), endpoint = ?self.endpoint_uri, uncompressed_len, compressed_len, inputs_written, "Flushing request.");

        vec![self.create_request(buffer).map(|req| (inputs_written, req))]
    }

    fn clear_encoded_inputs(&mut self) -> usize {
        let len = self.encoded_inputs.len();
        self.encoded_inputs.clear();
        len
    }

    async fn split_request(
        &mut self,
    ) -> Vec<Result<(usize, Request<FrozenChunkedBytesBuffer>), RequestBuilderError<E>>> {
        // Nothing to do if we have no encoded inputs.
        let mut requests = Vec::new();
        if self.encoded_inputs.is_empty() {
            return requests;
        }

        // We're going to attempt to split all of the previously-encoded inputs between two _new_ compressed payloads,
        // with the goal that each payload will be under the compressed size limit.
        //
        // We achieve this by temporarily consuming the "encoded inputs" buffer, feeding the first half of it back to
        // ourselves by re-encoding and then flushing, and then doing the same thing with the second half.  If either
        // half fails to properly encode, we give up entirely.
        //
        // We specifically manage the control flow so that we always restore the original "encoded inputs" buffer to
        // the builder (albeit cleared) before returning, so that we don't waste its allocation as it's been sized up
        // over time.
        //
        // We can do this by swapping it out with a new `Vec<E::Input>` since empty vectors don't allocate at all.
        let mut encoded_inputs = std::mem::take(&mut self.encoded_inputs);
        let encoded_inputs_pivot = encoded_inputs.len() / 2;

        let first_half_encoded_inputs = &encoded_inputs[0..encoded_inputs_pivot];
        let second_half_encoded_inputs = &encoded_inputs[encoded_inputs_pivot..];

        // TODO: We're duplicating functionality here between `encode`/`flush`, but this makes it a lot easier to skip
        // over the normal behavior that would do all the storing of encoded inputs, trying to split the payload, etc,
        // since we want to avoid that and avoid any recursion in general.
        //
        // We should consider if there's a better way to split out some of this into common methods or something.
        if let Some(request) = self.try_split_request(first_half_encoded_inputs).await {
            requests.push(request);
        }

        if let Some(request) = self.try_split_request(second_half_encoded_inputs).await {
            requests.push(request);
        }

        // Restore our original "encoded inputs" buffer before finishing up, but also clear it.
        encoded_inputs.clear();
        self.encoded_inputs = encoded_inputs;

        requests
    }

    async fn try_split_request(
        &mut self, inputs: &[E::Input],
    ) -> Option<Result<(usize, Request<FrozenChunkedBytesBuffer>), RequestBuilderError<E>>> {
        let mut uncompressed_len = 0;
        let mut compressor = create_compressor(&self.buffer_pool, self.compression_scheme).await;

        for input in inputs {
            // Encode each input and write it to our compressor.
            //
            // We skip any of the typical payload size checks here, because we already know we at least fit these
            // inputs into the previous attempted payload, so there's no reason to redo all of that here.
            self.scratch_buf.clear();
            if let Err(e) = self
                .encoder
                .encode(input, &mut self.scratch_buf)
                .context(FailedToEncode)
            {
                return Some(Err(e));
            }

            if let Err(e) = compressor.write_all(&self.scratch_buf[..]).await.context(Io) {
                return Some(Err(e));
            }

            uncompressed_len += self.scratch_buf.len();
        }

        // Make sure we haven't exceeded our uncompressed size limit.
        //
        // Again, this should never happen since we've already gone through this the first time but we're just being
        // extra sure here since the interface allows for it to happen. :shrug:
        if uncompressed_len > self.uncompressed_len_limit {
            let inputs_dropped = inputs.len();

            // TODO: Propagate the number of inputs dropped in the returned error itself rather than logging here.
            error!(
                encoder = E::encoder_name(),
                endpoint = ?self.endpoint_uri,
                uncompressed_len,
                inputs_dropped,
                "Uncompressed size limit exceeded while splitting request. This should never occur. Inputs have been dropped."
            );

            return None;
        }

        Some(
            self.finalize(compressor)
                .await
                .and_then(|buffer| self.create_request(buffer).map(|request| (inputs.len(), request))),
        )
    }

    async fn finalize(
        &self, mut compressor: Compressor<ChunkedBytesBuffer<O>>,
    ) -> Result<FrozenChunkedBytesBuffer, RequestBuilderError<E>> {
        compressor.shutdown().await.context(Io)?;
        let buffer = compressor.into_inner().freeze();
        let compressed_len = buffer.len();
        let compressed_limit = self.compressed_len_limit;
        if compressed_len > compressed_limit {
            return Err(RequestBuilderError::PayloadTooLarge {
                compressed_size_bytes: compressed_len,
                compressed_limit_bytes: compressed_limit,
            });
        }
        Ok(buffer)
    }

    fn create_request(
        &self, buffer: FrozenChunkedBytesBuffer,
    ) -> Result<Request<FrozenChunkedBytesBuffer>, RequestBuilderError<E>> {
        let mut builder = Request::builder()
            .method(self.encoder.endpoint_method())
            // We specifically use `self.endpoint_uri` here instead of `self.encoder.endpoint_uri()` because the
            // encoder's URI may have been overridden via `with_endpoint_uri_override`.
            .uri(self.endpoint_uri.clone())
            .header(http::header::CONTENT_TYPE, self.encoder.content_type());

        if let Some(content_encoding) = self.compressor.content_encoding() {
            builder = builder.header(http::header::CONTENT_ENCODING, content_encoding);
        }

        builder.body(buffer).context(Http)
    }
}

async fn create_compressor<O>(
    buffer_pool: &ChunkedBytesBufferObjectPool<O>, compression_scheme: CompressionScheme,
) -> Compressor<ChunkedBytesBuffer<O>>
where
    O: ObjectPool<Item = BytesBuffer> + 'static,
{
    let write_buffer = buffer_pool.acquire().await;
    Compressor::from_scheme(compression_scheme, write_buffer)
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;

    use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
    use http_body_util::BodyExt as _;
    use saluki_core::pooling::FixedSizeObjectPool;
    use saluki_io::{
        buf::{BytesBuffer, FixedSizeVec},
        compression::CompressionScheme,
    };

    use super::{EndpointEncoder, RequestBuilder, RequestBuilderError};

    fn create_request_builder_buffer_pool() -> FixedSizeObjectPool<BytesBuffer> {
        FixedSizeObjectPool::with_builder("test_pool", 8, || FixedSizeVec::with_capacity(64))
    }

    async fn create_request_builder(
        encoder: TestEncoder, compression_scheme: CompressionScheme,
    ) -> RequestBuilder<TestEncoder, FixedSizeObjectPool<BytesBuffer>> {
        let buffer_pool = create_request_builder_buffer_pool();
        RequestBuilder::new(encoder, buffer_pool, compression_scheme)
            .await
            .expect("should not fail to create request builder")
    }

    async fn create_no_compression_request_builder(
        encoder: TestEncoder,
    ) -> RequestBuilder<TestEncoder, FixedSizeObjectPool<BytesBuffer>> {
        create_request_builder(encoder, CompressionScheme::noop()).await
    }

    async fn create_zstd_compression_request_builder(
        encoder: TestEncoder,
    ) -> RequestBuilder<TestEncoder, FixedSizeObjectPool<BytesBuffer>> {
        create_request_builder(encoder, CompressionScheme::zstd_default()).await
    }

    async fn flush_and_validate_request(
        mut request_builder: RequestBuilder<TestEncoder, FixedSizeObjectPool<BytesBuffer>>,
        expected_request_body: Vec<u8>,
    ) {
        let mut requests = request_builder.flush().await;

        assert_eq!(requests.len(), 1);
        match requests.pop() {
            Some(Ok((_, request))) => {
                // We want to make sure the request uses the intended URI and HTTP method, and that it has the expected Content-Type.
                let expected_uri = request_builder.encoder().endpoint_uri();
                let actual_uri = request.uri();
                assert_eq!(
                    &expected_uri, actual_uri,
                    "flushed request had unexpected URI: expected {}, got {}",
                    expected_uri, actual_uri
                );

                let expected_method = request_builder.encoder().endpoint_method();
                let actual_method = request.method();
                assert_eq!(
                    expected_method, *actual_method,
                    "flushed request had unexpected method: expected {}, got {}",
                    expected_method, actual_method
                );

                let expected_content_type = request_builder.encoder().content_type();
                let actual_content_type = request.headers().get(http::header::CONTENT_TYPE).unwrap();
                assert_eq!(
                    expected_content_type, *actual_content_type,
                    "flushed request had unexpected Content-Type: expected {:?}, got {:?}",
                    expected_content_type, actual_content_type
                );

                // Now collect the request body and make sure it matches.
                let actual_request_body = request.into_body().collect().await.unwrap().to_bytes();
                assert_eq!(
                    expected_request_body, actual_request_body,
                    "flushed request body did not match expected body: expected {:?}, got {:?}",
                    expected_request_body, actual_request_body
                );
            }
            Some(Err(e)) => panic!("failed to create request: {}", e),
            None => panic!("no requests were created"),
        }
    }

    #[derive(Clone, Debug)]
    struct TestEncoder {
        compressed_size_limit: usize,
        uncompressed_size_limit: usize,
        endpoint_uri: &'static str,
        prefix: Option<&'static [u8]>,
        suffix: Option<&'static [u8]>,
        separator: Option<&'static [u8]>,
    }

    impl TestEncoder {
        fn new(compressed_size_limit: usize, uncompressed_size_limit: usize, endpoint_uri: &'static str) -> Self {
            Self {
                compressed_size_limit,
                uncompressed_size_limit,
                endpoint_uri,
                prefix: None,
                suffix: None,
                separator: None,
            }
        }

        fn with_delimiters(mut self, prefix: &'static [u8], suffix: &'static [u8], separator: &'static [u8]) -> Self {
            self.prefix = Some(prefix);
            self.suffix = Some(suffix);
            self.separator = Some(separator);
            self
        }
    }

    impl EndpointEncoder for TestEncoder {
        type Input = String;
        type EncodeError = Infallible;

        fn encoder_name() -> &'static str {
            "test_encoder"
        }

        fn compressed_size_limit(&self) -> usize {
            self.compressed_size_limit
        }

        fn uncompressed_size_limit(&self) -> usize {
            self.uncompressed_size_limit
        }

        fn encode(&self, input: &String, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
            // We just write the input string to the buffer as-is.
            buffer.extend_from_slice(input.as_bytes());
            Ok(())
        }

        fn get_payload_prefix(&self) -> Option<&'static [u8]> {
            self.prefix
        }

        fn get_payload_suffix(&self) -> Option<&'static [u8]> {
            self.suffix
        }

        fn get_input_separator(&self) -> Option<&'static [u8]> {
            self.separator
        }

        fn endpoint_uri(&self) -> Uri {
            PathAndQuery::from_static(self.endpoint_uri).into()
        }

        fn endpoint_method(&self) -> Method {
            Method::POST
        }

        fn content_type(&self) -> HeaderValue {
            HeaderValue::from_static("application/text")
        }
    }

    #[tokio::test]
    async fn basic() {
        // Create a basic request with no (un)compressed size limits, and no prefix/suffix.
        let encoder = TestEncoder::new(usize::MAX, usize::MAX, "/submit");
        let mut request_builder = create_no_compression_request_builder(encoder.clone()).await;

        // Without any prefix/suffix, and no compression, the request body should simply be concatenation of the inputs.
        let inputs = vec!["hello, world!".to_string(), "foo".to_string(), "bar".to_string()];
        let expected_body = inputs.iter().flat_map(|s| s.as_bytes()).copied().collect::<Vec<u8>>();

        // Encode the inputs, flush the request(s), and validate them.
        for input in inputs {
            request_builder.encode(input).await.unwrap();
        }

        flush_and_validate_request(request_builder, expected_body).await;
    }

    #[tokio::test]
    async fn with_prefix_and_suffix_single() {
        // Create a basic request with no (un)compressed size limits. Our encoder will write a prefix and suffix that
        // simulates writing a JSON payload, where the body is a JSON array with individual inputs as array items.
        let encoder = TestEncoder::new(usize::MAX, usize::MAX, "/submit").with_delimiters(b"[", b"]", b",");
        let mut request_builder = create_no_compression_request_builder(encoder.clone()).await;

        // We have to do a little extra work here to construct the expected body, since we have a prefix and suffix and
        // separator and all of that.
        let inputs = vec!["\"hello, world!\"".to_string()];
        let expected_body = format!("[{}]", inputs[0]);

        // Encode the inputs, flush the request(s), and validate them.
        for input in inputs {
            request_builder.encode(input).await.unwrap();
        }

        flush_and_validate_request(request_builder, expected_body.into_bytes()).await;
    }

    #[tokio::test]
    async fn with_prefix_and_suffix_multiple() {
        // Create a basic request with no (un)compressed size limits. Our encoder will write a prefix and suffix that
        // simulates writing a JSON payload, where the body is a JSON array with individual inputs as array items.
        let encoder = TestEncoder::new(usize::MAX, usize::MAX, "/submit").with_delimiters(b"[", b"]", b",");
        let mut request_builder = create_no_compression_request_builder(encoder.clone()).await;

        // We have to do a little extra work here to construct the expected body, since we have a prefix and suffix and
        // separator and all of that.
        let inputs = vec![
            "\"hello, world!\"".to_string(),
            "\"foo\"".to_string(),
            "\"bar\"".to_string(),
        ];
        let expected_body = format!("[{}]", inputs.join(","));

        // Encode the inputs, flush the request(s), and validate them.
        for input in inputs {
            request_builder.encode(input).await.unwrap();
        }

        flush_and_validate_request(request_builder, expected_body.into_bytes()).await;
    }

    #[tokio::test]
    async fn split_oversized_request() {
        // Generate some inputs that will exceed the compressed size limit.
        let input1 = "mary had a little lamb and its fleece was white as snow".to_string();
        let input2 = "and everywhere that mary went the lamb was sure to go".to_string();
        let input3 = "it followed her to school one day which was against the rule".to_string();
        let input4 = "it made the children laugh and play to see a lamb at school".to_string();

        // Create a regular ol' request builder with unlimited (un)compressed size limits, to ensure we can write all
        // four inputs before trying to flush.
        let encoder = TestEncoder::new(usize::MAX, usize::MAX, "/submit");
        let mut request_builder = create_zstd_compression_request_builder(encoder).await;

        // Encode the inputs, which should all fit into the request payload.
        let inputs = vec![input1, input2, input3, input4];
        for input in inputs {
            match request_builder.encode(input).await {
                Ok(None) => {}
                Ok(Some(_)) => panic!("initial encode should never fail to fit encoded input payload"),
                Err(e) => panic!("initial encode should never fail: {}", e),
            }
        }

        // Now we attempt to flush, but first, we'll adjust our limits to force the builder to split the request,
        // specifically the compressed size limit.
        //
        // We've chosen 96 because it's just under where the compressor should land when compressing all four inputs.
        // This value may need to change in the future if we change to a different compression algorithm.
        request_builder.set_custom_len_limits(usize::MAX, 96);

        let requests = request_builder.flush().await;
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn obeys_max_inputs_per_payload() {
        // Generate some simple inputs.
        let input1 = "input1".to_string();
        let input2 = "input2".to_string();
        let input3 = "input3".to_string();

        // Create a regular ol' request builder with unlimited (un)compressed size limits, and no limit on the number of
        // inputs per payload.
        //
        // We should be able to encode three inputs without issue.
        let encoder = TestEncoder::new(usize::MAX, usize::MAX, "/submit");
        let mut request_builder = create_no_compression_request_builder(encoder.clone()).await;

        assert_eq!(None, request_builder.encode(input1.clone()).await.unwrap());
        assert_eq!(None, request_builder.encode(input2.clone()).await.unwrap());
        assert_eq!(None, request_builder.encode(input3.clone()).await.unwrap());

        // Now create a request builder with unlimited (un)compressed size limits, but a limit of 2 inputs per payload.
        //
        // We should only be able to encode two of the inputs before we're signaled to flush.
        let mut request_builder = create_no_compression_request_builder(encoder).await;
        request_builder.with_max_inputs_per_payload(2);

        assert_eq!(None, request_builder.encode(input1).await.unwrap());
        assert_eq!(None, request_builder.encode(input2).await.unwrap());
        assert_eq!(Some(input3.clone()), request_builder.encode(input3).await.unwrap());

        // Since we know we could fit the same three inputs in the first request builder when there was no limit on the
        // number of inputs per payload, we know we're not being instructed to flush here due to hitting (un)compressed
        // size limits.
    }

    #[tokio::test]
    async fn override_endpoint_uri() {
        // Create a request builder with a specific endpoint URI.
        let encoder = TestEncoder::new(usize::MAX, usize::MAX, "/submit");
        let mut request_builder = create_no_compression_request_builder(encoder.clone()).await;

        // Override the endpoint URI.
        request_builder.with_endpoint_uri_override("/override");

        // Encode a single input and then flush the builder, ensuring the request has the overridden endpoint URI.
        request_builder.encode("input".to_string()).await.unwrap();

        let mut requests = request_builder.flush().await;
        assert_eq!(requests.len(), 1);

        // Check that the request was created with the overridden endpoint URI.
        match requests.pop() {
            Some(Ok((_, request))) => {
                assert_eq!(request.uri().path(), "/override");
            }
            Some(Err(e)) => panic!("failed to create request: {}", e),
            None => panic!("no requests were created"),
        }
    }

    #[tokio::test]
    async fn uncompressed_size_limit_too_small() {
        // Make sure that we can't build a request builder with an uncompressed size limit that is smaller than the
        // minimum payload size: we calculate the minimum payload size as the sum of the configured prefix and suffix,
        // plus one.
        //
        // This is, conceptually, the smallest possible payload that could be written that isn't empty.
        let prefix = b"[";
        let suffix = b"]";

        let buffer_pool = create_request_builder_buffer_pool();
        let encoder =
            TestEncoder::new(usize::MAX, prefix.len() + suffix.len(), "/submit").with_delimiters(prefix, suffix, b"");

        let maybe_request_builder = RequestBuilder::new(encoder, buffer_pool, CompressionScheme::zstd_default()).await;
        match maybe_request_builder {
            Ok(_) => panic!("building request builder should not succeed"),
            Err(e) => match e {
                RequestBuilderError::UncompressedSizeLimitTooLow {
                    uncompressed_size_limit,
                    prefix_len,
                    suffix_len,
                } => {
                    assert_eq!(uncompressed_size_limit, prefix.len() + suffix.len());
                    assert_eq!(prefix_len, prefix.len());
                    assert_eq!(suffix_len, suffix.len());
                }
                _ => panic!("expected UncompressedSizeLimitTooLow error, got: {:?}", e),
            },
        }
    }
}
