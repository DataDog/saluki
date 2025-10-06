#![allow(dead_code)]

use std::io;

use http::{HeaderValue, Method, Request, Uri};
use saluki_common::buf::{ChunkedBytesBuffer, FrozenChunkedBytesBuffer};
use saluki_io::compression::*;
use snafu::{ResultExt, Snafu};
use tokio::io::AsyncWriteExt as _;
use tracing::{debug, error, trace, warn};

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
    fn encode(&mut self, input: &Self::Input, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError>;

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
            // Payloads that are oversized can be recovered from, because all we do is discard the payload without
            // leaving the builder/encoder in an inconsistent state.
            Self::PayloadTooLarge { .. } => true,

            // Encoder _failures_, or I/O errors, or misconfigurations, are either static (trying again won't help)
            // or they are related to ending up in an inconsistent state that we cannot reason about and automatically
            // recover from.
            _ => false,
        }
    }
}

/// Generic builder for creating HTTP requests with payloads consisting of encoded and compressed input events.
pub struct RequestBuilder<E>
where
    E: EndpointEncoder,
{
    encoder: E,
    scratch_buf: Vec<u8>,
    buffer_chunk_size: usize,
    compression_scheme: CompressionScheme,
    compressor: Compressor<ChunkedBytesBuffer>,
    compression_estimator: CompressionEstimator,
    uncompressed_len: usize,
    uncompressed_len_prefix_suffix: usize,
    compressed_len_limit: usize,
    uncompressed_len_limit: usize,
    max_inputs_per_payload: usize,
    encoded_inputs: Vec<E::Input>,
}

impl<E> RequestBuilder<E>
where
    E: EndpointEncoder,
{
    /// Creates a new `RequestBuilder` with the given encoder and compression scheme.
    ///
    /// The encoder will be used to encode input events as well as help construct the resulting HTTP requests.
    pub async fn new(
        encoder: E, compression_scheme: CompressionScheme, buffer_chunk_size: usize,
    ) -> Result<Self, RequestBuilderError<E>> {
        let compressed_len_limit = encoder.compressed_size_limit();
        let uncompressed_len_limit = encoder.uncompressed_size_limit();

        // Make sure the uncompressed size limit is large enough to accommodate the prefix and suffix and an additional
        // byte: this is the smallest possible valid payload that could conceivably be written, and so we have to be
        // able to at least fit that.
        let prefix_len = encoder.get_payload_prefix().map_or(0, |p| p.len());
        let suffix_len = encoder.get_payload_suffix().map_or(0, |s| s.len());
        let uncompressed_len_prefix_suffix = prefix_len + suffix_len;
        if uncompressed_len_limit < uncompressed_len_prefix_suffix + 1 {
            return Err(RequestBuilderError::UncompressedSizeLimitTooLow {
                uncompressed_size_limit: uncompressed_len_limit,
                prefix_len,
                suffix_len,
            });
        }

        let compressor = create_compressor(compression_scheme, buffer_chunk_size);
        debug!(
            encoder = E::encoder_name(),
            "WACKTEST7: rb_new buffer_chunk_size={} uncompressed_limit={} compressed_limit={}",
            buffer_chunk_size,
            uncompressed_len_limit,
            compressed_len_limit
        );
        Ok(Self {
            encoder,
            scratch_buf: Vec::with_capacity(SCRATCH_BUF_CAPACITY),
            buffer_chunk_size,
            compression_scheme,
            compressor,
            compression_estimator: CompressionEstimator::default(),
            uncompressed_len: 0,
            uncompressed_len_prefix_suffix,
            compressed_len_limit,
            uncompressed_len_limit,
            max_inputs_per_payload: usize::MAX,
            encoded_inputs: Vec::new(),
        })
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

    fn uncompressed_len(&self) -> usize {
        // We specifically track the payload prefix/suffix length separately from the uncompressed length, so that we
        // can account for it when encoding: we wouldn't have written the suffix yet, but we must account for the suffix
        // to know if the finalized payload will have an uncompressed size that exceeds the limit or not.
        self.uncompressed_len + self.uncompressed_len_prefix_suffix
    }

    async fn prepare_for_write(&mut self) -> Result<(), io::Error> {
        // If we haven't written any inputs yet, and we have a payload prefix, we need to write that.
        if self.uncompressed_len == 0 {
            if let Some(prefix) = self.encoder.get_payload_prefix() {
                self.scratch_buf.clear();
                self.scratch_buf.extend_from_slice(prefix);

                // Don't update `self.uncompressed_len` since we account for the payload prefix/suffix length through `self.uncompressed_len_prefix_suffix`.
                self.flush_scratch_buffer(false).await?;
            }
        } else {
            // Similarly, if we've already written inputs, and we have an input separator, we need to write that.
            if let Some(separator) = self.encoder.get_input_separator() {
                self.scratch_buf.clear();
                self.scratch_buf.extend_from_slice(separator);
                self.flush_scratch_buffer(true).await?;
            }
        }

        Ok(())
    }

    /// Flushes the scratch buffer to the compressor.
    ///
    /// If `update_compressed_len` is `true`, the uncompressed length will be updated to reflect the size of the data
    /// written.
    async fn flush_scratch_buffer(&mut self, update_uncompressed_len: bool) -> Result<(), io::Error> {
        let buf = self.scratch_buf.as_slice();
        self.compressor.write_all(buf).await?;
        self.compression_estimator.track_write(&self.compressor, buf.len());

        if update_uncompressed_len {
            self.uncompressed_len += buf.len();
        }

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
            trace!("Maximum number of inputs per payload reached.");
            debug!(
                encoder = E::encoder_name(),
                "WACKTEST7: rb_max_inputs_per_payload len={} max={}",
                self.encoded_inputs.len(),
                self.max_inputs_per_payload
            );
            return Ok(Some(input));
        }

        // Try encoding the input.
        //
        // If the input can't fit into the current request payload based on the uncompressed size limit, or isn't likely
        // to fit, `false` is returned, which signals us to give the input back to the caller so they can flush the
        // current payload and then try again.
        //
        // Otherwise, we wrote the encoded input successfully so we'll hold on to that input for now in case we need to
        // split the payload later.
        if self.encode_inner(&input).await? {
            self.encoded_inputs.push(input);
            Ok(None)
        } else {
            Ok(Some(input))
        }
    }

    /// Internal implementation of `encode`.
    ///
    /// This method excludes any specific edge case/error handling (such as if the input is valid, or asserting we
    /// haven't hit input limits), and avoids any of the logic that supports request splitting. It is written this way
    /// so that it can be used in the request splitting logic itself without any thorny recursion issues.
    async fn encode_inner(&mut self, input: &E::Input) -> Result<bool, RequestBuilderError<E>> {
        // Write any configured prefix/input separator, if necessary.
        self.prepare_for_write().await.context(Io)?;

        // Encode the input and then see if it will fit into the current request payload.
        //
        // If not, we return the original input, signaling to the caller that they need to flush the current request
        // payload before encoding additional inputs.
        self.scratch_buf.clear();
        self.encoder
            .encode(input, &mut self.scratch_buf)
            .context(FailedToEncode)?;

        // If the input can't fit into the current request payload based on the uncompressed size limit, or isn't likely
        // to fit into the current request payload based on the estimated compressed size limit, then return it to the
        // caller: this indicates that a flush must happen before trying to encode the same input again.
        //
        // We specifically include the prefix/suffix length separately, as this lets us account for them before having
        // actually written the suffix, such that our calculation of the uncompressed length of the finalized payload is
        // accurate.
        let encoded_len = self.scratch_buf.len();
        let would_exceed_uncompressed_limit = self.uncompressed_len() + encoded_len > self.uncompressed_len_limit;
        let likely_exceeds_compressed_limit = self
            .compression_estimator
            .would_write_exceed_threshold(encoded_len, self.compressed_len_limit);
        if would_exceed_uncompressed_limit || likely_exceeds_compressed_limit {
            trace!(
                encoder = E::encoder_name(),
                endpoint = ?self.encoder.endpoint_uri(),
                encoded_len,
                uncompressed_len = self.uncompressed_len(),
                estimated_compressed_len = self.compression_estimator.estimated_len(),
                "Input would exceed endpoint size limits."
            );
            debug!(
                encoder = E::encoder_name(),
                "WACKTEST7: rb_encode_deferred encoded_len={} uncompressed_len={} est_compressed_len={} max_inputs={} encoded_inputs_len={}",
                encoded_len,
                self.uncompressed_len(),
                self.compression_estimator.estimated_len(),
                self.max_inputs_per_payload,
                self.encoded_inputs.len()
            );
            return Ok(false);
        }

        // Write the scratch buffer to the compressor.
        self.flush_scratch_buffer(true).await.context(Io)?;

        trace!(
            encoder = E::encoder_name(),
            endpoint = ?self.encoder.endpoint_uri(),
            encoded_len,
            uncompressed_len = self.uncompressed_len(),
            estimated_compressed_len = self.compression_estimator.estimated_len(),
            "Wrote encoded input to compressor."
        );
        debug!(
            encoder = E::encoder_name(),
            "WACKTEST7: rb_encoded encoded_len={} uncompressed_len={} est_compressed_len={} inputs_in_payload={}"
            ,
            encoded_len,
            self.uncompressed_len(),
            self.compression_estimator.estimated_len(),
            self.encoded_inputs.len() + 1
        );

        Ok(true)
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

        // Flush the compressor and get the compressed payload, and the uncompressed length of the payload.
        let (uncompressed_len, compressed_buf) = match self.flush_inner().await {
            Ok((uncompressed_len, buffer)) => (uncompressed_len, buffer),
            Err(e) => return vec![Err(e)],
        };

        // Make sure we haven't exceeded our compressed size limit, otherwise we'll need to split the request.
        let compressed_len = compressed_buf.len();
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
        debug!(
            encoder = E::encoder_name(),
            endpoint = ?self.encoder.endpoint_uri(),
            uncompressed_len,
            compressed_len,
            inputs_written,
            "Flushing request."
        );
        debug!(
            encoder = E::encoder_name(),
            "WACKTEST8: rb_flush uncompressed_len={} compressed_len={} inputs_written={} encoded_inputs_capacity={}",
            uncompressed_len,
            compressed_len,
            inputs_written,
            self.encoded_inputs.capacity()
        );

        vec![self.create_request(compressed_buf).map(|req| (inputs_written, req))]
    }

    /// Internal implementation of `flush`.
    ///
    /// This method excludes any specific edge case/error handling (such as checking if the (un)compressed size limits
    /// are exceeded), and does not handle request splitting, as it is meant to be used in the request splitting logic
    /// itself.
    async fn flush_inner(&mut self) -> Result<(usize, FrozenChunkedBytesBuffer), RequestBuilderError<E>> {
        // If we have a payload suffix configured, write it now.
        if let Some(suffix) = self.encoder.get_payload_suffix() {
            self.scratch_buf.clear();
            self.scratch_buf.extend_from_slice(suffix);
            self.flush_scratch_buffer(false).await.context(Io)?;
        }

        // Clear our internal state and finalize the compressor. We do it in this order so that if finalization fails,
        // somehow, the request builder is in a default state and encoding can be attempted again.
        let uncompressed_len = self.uncompressed_len();
        self.uncompressed_len = 0;

        self.compression_estimator.reset();

        let new_compressor = create_compressor(self.compression_scheme, self.buffer_chunk_size);
        let mut compressor = std::mem::replace(&mut self.compressor, new_compressor);
        if let Err(e) = compressor.flush().await.context(Io) {
            let inputs_dropped = self.clear_encoded_inputs();

            // TODO: Propagate the number of inputs dropped in the returned error itself rather than logging here.
            error!(
                encoder = E::encoder_name(),
                endpoint = ?self.encoder.endpoint_uri(),
                inputs_dropped,
                "Failed to finalize compressor while building request. Inputs have been dropped."
            );

            return Err(e);
        }

        if let Err(e) = compressor.shutdown().await.context(Io) {
            let inputs_dropped = self.clear_encoded_inputs();

            // TODO: Propagate the number of inputs dropped in the returned error itself rather than logging here.
            error!(
                encoder = E::encoder_name(),
                endpoint = ?self.encoder.endpoint_uri(),
                inputs_dropped,
                "Failed to finalize compressor while building request. Inputs have been dropped."
            );

            return Err(e);
        }

        let frozen = compressor.into_inner().freeze();
        debug!(
            encoder = E::encoder_name(),
            "WACKTEST8: rb_flush_inner uncompressed_len={} frozen_len={} frozen_chunks={}",
            uncompressed_len,
            frozen.len(),
            frozen.chunk_count()
        );
        Ok((uncompressed_len, frozen))
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
            warn!(
                encoder = E::encoder_name(),
                endpoint = ?self.encoder.endpoint_uri(),
                "Tried to split request with no encoded inputs."
            );
            return requests;
        }

        trace!(
            encoder = E::encoder_name(),
            endpoint = ?self.encoder.endpoint_uri(),
            encoded_inputs = self.encoded_inputs.len(),
            "Starting request split operation.",
        );

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

        if let Some(request) = self.try_split_request(first_half_encoded_inputs).await {
            requests.push(request);
        }

        if let Some(request) = self.try_split_request(second_half_encoded_inputs).await {
            requests.push(request);
        }

        // Restore our original "encoded inputs" buffer before finishing up, but also clear it.
        encoded_inputs.clear();
        self.encoded_inputs = encoded_inputs;

        trace!(
            encoder = E::encoder_name(),
            endpoint = ?self.encoder.endpoint_uri(),
            "Finished splitting oversized request. Generated {} subrequest(s).",
            requests.len(),
        );

        requests
    }

    async fn try_split_request(
        &mut self, inputs: &[E::Input],
    ) -> Option<Result<(usize, Request<FrozenChunkedBytesBuffer>), RequestBuilderError<E>>> {
        trace!(
            encoder = E::encoder_name(),
            endpoint = ?self.encoder.endpoint_uri(),
            encoded_inputs = inputs.len(),
            "Starting request split suboperation.",
        );

        for input in inputs {
            // Encode each input and write it to our compressor.
            //
            // We skip any of the typical payload size checks here, because we already know we at least fit these
            // inputs into the previous attempted payload, so there's no reason to redo all of that here.
            match self.encode_inner(input).await {
                Ok(true) => {}
                Ok(false) => {
                    // If we can't encode the input, we need to stop here and return the input to the caller.
                    return None;
                }
                Err(e) => {
                    // If we get an error, we need to stop here and return the error to the caller.
                    return Some(Err(e));
                }
            }
        }

        let (uncompressed_len, compressed_buf) = match self.flush_inner().await {
            Ok((uncompressed_len, buffer)) => (uncompressed_len, buffer),
            Err(e) => return Some(Err(e)),
        };

        // Make sure we haven't exceeded our uncompressed size limit.
        //
        // Again, this should never happen since we've already gone through this the first time but we're just being
        // extra sure here since the interface allows for it to happen. :shrug:
        if uncompressed_len > self.uncompressed_len_limit {
            let inputs_dropped = inputs.len();

            // TODO: Propagate the number of inputs dropped in the returned error itself rather than logging here.
            error!(
                encoder = E::encoder_name(),
                endpoint = ?self.encoder.endpoint_uri(),
                uncompressed_len,
                inputs_dropped,
                "Uncompressed size limit exceeded while splitting request. This should never occur. Inputs have been dropped."
            );

            return None;
        }

        // Finally, make sure we haven't exceeded our compressed size limit.
        let compressed_len = compressed_buf.len();
        let compressed_limit = self.compressed_len_limit;
        if compressed_len > compressed_limit {
            return Some(Err(RequestBuilderError::PayloadTooLarge {
                compressed_size_bytes: compressed_len,
                compressed_limit_bytes: compressed_limit,
            }));
        }

        debug!(
            encoder = E::encoder_name(),
            "WACKTEST9: rb_try_split_request inputs={} uncompressed_len={} compressed_len={}"
            ,
            inputs.len(),
            uncompressed_len,
            compressed_len
        );
        Some(
            self.create_request(compressed_buf)
                .map(|request| (inputs.len(), request)),
        )
    }

    fn create_request(
        &self, buffer: FrozenChunkedBytesBuffer,
    ) -> Result<Request<FrozenChunkedBytesBuffer>, RequestBuilderError<E>> {
        let mut builder = Request::builder()
            .method(self.encoder.endpoint_method())
            .uri(self.encoder.endpoint_uri())
            .header(http::header::CONTENT_TYPE, self.encoder.content_type());

        if let Some(content_encoding) = self.compressor.content_encoding() {
            builder = builder.header(http::header::CONTENT_ENCODING, content_encoding);
        }

        builder.body(buffer).context(Http)
    }
}

fn create_compressor(
    compression_scheme: CompressionScheme, buffer_chunk_size: usize,
) -> Compressor<ChunkedBytesBuffer> {
    Compressor::from_scheme(compression_scheme, ChunkedBytesBuffer::new(buffer_chunk_size))
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, convert::Infallible};

    use http::{uri::PathAndQuery, HeaderValue, Method, Uri};
    use http_body_util::BodyExt as _;
    use proptest::{collection::vec_deque as arb_vecdeque, prelude::*, string::string_regex};
    use saluki_io::compression::CompressionScheme;

    use super::{EndpointEncoder, RequestBuilder, RequestBuilderError};
    use crate::common::datadog::io::RB_BUFFER_CHUNK_SIZE;

    async fn create_request_builder(
        encoder: TestEncoder, compression_scheme: CompressionScheme,
    ) -> RequestBuilder<TestEncoder> {
        RequestBuilder::new(encoder, compression_scheme, RB_BUFFER_CHUNK_SIZE)
            .await
            .expect("should not fail to create request builder")
    }

    async fn create_no_compression_request_builder(encoder: TestEncoder) -> RequestBuilder<TestEncoder> {
        create_request_builder(encoder, CompressionScheme::noop()).await
    }

    async fn create_zstd_compression_request_builder(encoder: TestEncoder) -> RequestBuilder<TestEncoder> {
        create_request_builder(encoder, CompressionScheme::zstd_default()).await
    }

    async fn flush_and_validate_requests(
        mut request_builder: RequestBuilder<TestEncoder>, expected_request_bodies: Vec<String>,
    ) {
        let requests = request_builder.flush().await;
        assert_eq!(
            requests.len(),
            expected_request_bodies.len(),
            "got {} requests after flush, but expected {}",
            requests.len(),
            expected_request_bodies.len()
        );

        for (request, expected_request_body) in requests.into_iter().zip(expected_request_bodies) {
            let (request, expected_request_body) = match request {
                Ok((_, request)) => (request, expected_request_body),
                Err(e) => panic!("failed to create request: {}", e),
            };

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
                expected_request_body.as_bytes(),
                actual_request_body,
                "flushed request body did not match expected body: expected {:?}, got {:?}",
                expected_request_body,
                actual_request_body
            );
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

        fn encode(&mut self, input: &String, buffer: &mut Vec<u8>) -> Result<(), Self::EncodeError> {
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
    async fn basic_not_delimited() {
        // Create a basic request builder with no (un)compressed size limits, and no prefix/suffix.
        let encoder = TestEncoder::new(usize::MAX, usize::MAX, "/submit");
        let mut request_builder = create_no_compression_request_builder(encoder.clone()).await;

        // Without any prefix/suffix, and no compression, the request body should simply be concatenation of the inputs.
        let inputs = vec!["hello, world!".to_string(), "foo".to_string(), "bar".to_string()];
        let expected_body = inputs.join("");

        // Encode the inputs, flush the request(s), and validate them.
        for input in inputs {
            request_builder.encode(input).await.unwrap();
        }

        flush_and_validate_requests(request_builder, vec![expected_body]).await;
    }

    #[tokio::test]
    async fn basic_delimited_single() {
        // Create a basic request builder with no (un)compressed size limits. Our encoder will write a prefix and suffix
        // that simulates writing a JSON payload, where the body is a JSON array with individual inputs as array items.
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

        flush_and_validate_requests(request_builder, vec![expected_body]).await;
    }

    #[tokio::test]
    async fn basic_delimited_multiple() {
        // Create a basic request builder with no (un)compressed size limits. Our encoder will write a prefix and suffix
        // that simulates writing a JSON payload, where the body is a JSON array with individual inputs as array items.
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

        flush_and_validate_requests(request_builder, vec![expected_body]).await;
    }

    #[tokio::test]
    async fn split_oversized_request_not_delimited() {
        // Generate some inputs that will exceed the compressed size limit when combined.
        let inputs = vec![
            "mary had a little lamb and its fleece was white as snow".to_string(),
            "and everywhere that mary went the lamb was sure to go".to_string(),
            "it followed her to school one day which was against the rule".to_string(),
            "it made the children laugh and play to see a lamb at school".to_string(),
        ];

        // Create a basic request builder with no (un)compressed size limits, and no prefix/suffix.
        let encoder = TestEncoder::new(usize::MAX, usize::MAX, "/submit");
        let mut request_builder = create_no_compression_request_builder(encoder).await;

        // Encode the inputs, which should all fit into the request payload.
        for input in &inputs {
            request_builder.encode(input.clone()).await.unwrap();
        }

        // Now we attempt to flush, but first, we'll adjust our limits to force the builder to split the request,
        // specifically the compressed size limit.
        //
        // All we care about is triggering the logic, so just set the compressed size limit to the sum of all inputs,
        // minus one, to ensure it's smaller than the actual compressed size.
        request_builder.set_custom_len_limits(usize::MAX, inputs.iter().map(|s| s.len()).sum::<usize>() - 1);

        let expected_request_bodies = vec![
            format!("{}{}", inputs[0], inputs[1]),
            format!("{}{}", inputs[2], inputs[3]),
        ];
        flush_and_validate_requests(request_builder, expected_request_bodies).await;
    }

    #[tokio::test]
    async fn split_oversized_request_delimited() {
        // Generate some inputs that will exceed the compressed size limit when combined.
        let inputs = vec![
            "mary had a little lamb and its fleece was white as snow".to_string(),
            "and everywhere that mary went the lamb was sure to go".to_string(),
            "it followed her to school one day which was against the rule".to_string(),
            "it made the children laugh and play to see a lamb at school".to_string(),
        ];

        // Create a basic request builder with no (un)compressed size limits, and no prefix/suffix.
        let encoder = TestEncoder::new(usize::MAX, usize::MAX, "/submit").with_delimiters(b"[", b"]", b",");
        let mut request_builder = create_no_compression_request_builder(encoder).await;

        // Encode the inputs, which should all fit into the request payload.
        for input in &inputs {
            request_builder.encode(input.clone()).await.unwrap();
        }

        // Now we attempt to flush, but first, we'll adjust our limits to force the builder to split the request,
        // specifically the compressed size limit.
        //
        // All we care about is triggering the logic, so just set the compressed size limit to the sum of all inputs,
        // which is shorter than what it will generate when including the prefix, suffix, and input separators.
        request_builder.set_custom_len_limits(usize::MAX, inputs.iter().map(|s| s.len()).sum());

        let expected_request_bodies = vec![
            format!("[{},{}]", inputs[0], inputs[1]),
            format!("[{},{}]", inputs[2], inputs[3]),
        ];
        flush_and_validate_requests(request_builder, expected_request_bodies).await;
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
    async fn uncompressed_size_limit_too_small() {
        // Make sure that we can't build a request builder with an uncompressed size limit that is smaller than the
        // minimum payload size: we calculate the minimum payload size as the sum of the configured prefix and suffix,
        // plus one.
        //
        // This is, conceptually, the smallest possible payload that could be written that isn't empty.
        let prefix = b"[";
        let suffix = b"]";

        let encoder =
            TestEncoder::new(usize::MAX, prefix.len() + suffix.len(), "/submit").with_delimiters(prefix, suffix, b"");

        let maybe_request_builder =
            RequestBuilder::new(encoder, CompressionScheme::zstd_default(), RB_BUFFER_CHUNK_SIZE).await;
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

    // Property test min/max size constants used for both generating inputs but also calculating derived sizes.
    //
    // These constants are set in a way where we generate big enough inputs to actually cause the compressor to flush output
    // data to the underlying writer before request payloads are flushed, so that we properly exercise the compression
    // estimator.
    const PROP_TEST_ARB_PAYLOAD_MIN_LEN: usize = 512;
    const PROP_TEST_ARB_PAYLOAD_MAX_LEN: usize = 2048;
    const PROP_TEST_ARB_PAYLOAD_AVG_LEN: usize =
        PROP_TEST_ARB_PAYLOAD_MIN_LEN + ((PROP_TEST_ARB_PAYLOAD_MAX_LEN - PROP_TEST_ARB_PAYLOAD_MIN_LEN) / 2);

    const PROP_TEST_MIN_PAYLOADS: usize = 64;
    const PROP_TEST_MAX_PAYLOADS: usize = 192;
    const PROP_TEST_AVG_PAYLOADS: usize =
        PROP_TEST_MIN_PAYLOADS + ((PROP_TEST_MAX_PAYLOADS - PROP_TEST_MIN_PAYLOADS) / 2);

    fn arb_payloads() -> impl Strategy<Value = VecDeque<String>> {
        // Very crude approximation of a random binary/hexadecimal-y payload.
        let payload_regex = format!(
            "[a-zA-Z0-9]{{{},{}}}",
            PROP_TEST_ARB_PAYLOAD_MIN_LEN, PROP_TEST_ARB_PAYLOAD_MAX_LEN
        );
        let payload_strategy = string_regex(&payload_regex).expect("should not fail to create arb_payload regex");

        arb_vecdeque(payload_strategy, PROP_TEST_MIN_PAYLOADS..=PROP_TEST_MAX_PAYLOADS)
    }

    #[test_strategy::proptest(async = "tokio")]
    #[cfg_attr(miri, ignore)]
    async fn property_test_encode_and_flush(#[strategy(arb_payloads())] mut inputs: VecDeque<String>) {
        let original_inputs_len = inputs.len();

        // Create our request builder with lowered limits just so the test can actually exercise them.
        //
        // We calculate these limits based on the input sizing to try and ensure that we have a strong chance of
        // having to actually split the payload into subpayloads due to exceeding the compressed length limit. We do
        // this by setting the compressed limit to 40% of the average payload length multiplied by the number of inputs.
        //
        // Given the randomness of the inputs, and thus their lack of compressibility, this should yield many splits.
        let uncompressed_limit = (PROP_TEST_ARB_PAYLOAD_MAX_LEN * original_inputs_len) * 2;
        let compressed_limit = ((PROP_TEST_ARB_PAYLOAD_AVG_LEN * original_inputs_len) as f64 * 0.4) as usize;

        let encoder = TestEncoder::new(compressed_limit, uncompressed_limit, "/fake/endpoint");
        let mut request_builder = RequestBuilder::new(encoder, CompressionScheme::zstd_default(), 2048)
            .await
            .expect("should not fail to create request builder");

        // Set our "maximum inputs per payload" limit to the average number of inputs to also ensure we have a good
        // chance of exercising that logic as well.
        request_builder.with_max_inputs_per_payload(PROP_TEST_AVG_PAYLOADS);

        // Go through for each input, and encode it. We also capture a copy of the input for later verification.
        //
        // If we're given back the item during encoding and a flush is indicated, we do trigger a flush... but we
        // take the requests and just dump out their contents for later verification.
        let mut flushed_inputs_len = 0;

        while let Some(input) = inputs.pop_front() {
            let original_input = match request_builder.encode(input).await {
                Ok(None) => continue,
                Ok(Some(original_input)) => original_input,
                Err(_) => panic!("should not fail to encode input in general"),
            };

            // We were instructed to flush, so we do so.
            let had_prior_flushes = flushed_inputs_len > 0;
            let mut total_oversized_subrequests = 0;
            let requests = request_builder.flush().await;
            for request in requests {
                match request {
                    Ok((events, _request)) => {
                        flushed_inputs_len += events;
                    }
                    Err(e) => match e {
                        RequestBuilderError::PayloadTooLarge { .. } => total_oversized_subrequests += 1,
                        _ => panic!("should not fail to flush requests (intermediate): {:?}", e),
                    },
                }
            }

            // TODO: Currently, we only support splitting a request into two subrequests.
            //
            // If our test here encodes a bunch of metrics that don't require flushing until the very end, we have
            // no idea how much compressed data we'll end up with. If we do the compression and we've exceeded the limit,
            // that's fine, and we should try splitting... but if we, for example, have 2-3x more compressed output than
            // the limit, then we're almost certainly going to be unable to actually split it in half without again
            // exceeding the limit for the subrequests.
            //
            // If we hadn't yet flushed anything prior to getting here, and we failed due to an oversized split payload,
            // then we simply reject this test because we know that we can't split it in half without exceeding the limit.
            // Once we remove the limitation of only being able to split a request in half, we'll remove this escape hatch.
            if !had_prior_flushes {
                prop_assume!(total_oversized_subrequests == 0);
            }

            // Try again to encode our input after flushing.
            match request_builder.encode(original_input).await {
                Ok(None) => continue,
                Ok(Some(_)) => panic!("should not fail to encode an input after flushing"),
                Err(_) => panic!("should not fail to encode input in general"),
            };
        }

        // One final flush to capture anything still in the request builder.
        let had_prior_flushes = flushed_inputs_len > 0;
        let mut total_oversized_subrequests = 0;
        let requests = request_builder.flush().await;
        for request in requests {
            match request {
                Ok((events, _request)) => {
                    flushed_inputs_len += events;
                }
                Err(e) => match e {
                    RequestBuilderError::PayloadTooLarge { .. } => total_oversized_subrequests += 1,
                    _ => panic!("should not fail to flush requests (final)"),
                },
            }
        }

        // TODO: Currently, we only support splitting a request into two subrequests.
        //
        // If our test here encodes a bunch of metrics that don't require flushing until the very end, we have
        // no idea how much compressed data we'll end up with. If we do the compression and we've exceeded the limit,
        // that's fine, and we should try splitting... but if we, for example, have 2-3x more compressed output than
        // the limit, then we're almost certainly going to be unable to actually split it in half without again
        // exceeding the limit for the subrequests.
        //
        // If we hadn't yet flushed anything prior to getting here, and we failed due to an oversized split payload,
        // then we simply reject this test because we know that we can't split it in half without exceeding the limit.
        // Once we remove the limitation of only being able to split a request in half, we'll remove this escape hatch.
        if !had_prior_flushes {
            prop_assume!(total_oversized_subrequests == 0);
        }

        prop_assert_eq!(original_inputs_len, flushed_inputs_len);
    }
}
