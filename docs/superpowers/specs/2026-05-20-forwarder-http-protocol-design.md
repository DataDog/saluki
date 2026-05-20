# Support `forwarder_http_protocol` HTTP/1.1 mode

## Context

Issue 1361 reports that ADP treats the core Agent setting `forwarder_http_protocol: http1` as a no-op. The current HTTP client always enables both HTTP/1.1 and HTTP/2 in ALPN, which matches the core Agent's `auto` mode but cannot force HTTP/1.1 only.

## Design

We will add a protocol preference to the HTTP client path used by the Datadog forwarder:

- `auto` preserves current behavior: advertise HTTP/2 and HTTP/1.1 through ALPN and allow negotiation.
- `http1` advertises only `http/1.1` through ALPN and configures the client for HTTP/1.1-only behavior where the HTTP stack exposes that control.

The implementation should follow existing configuration and builder patterns rather than introducing a separate client stack.

## Testing

We will add a regression test before implementation that demonstrates `forwarder_http_protocol: http1` changes HTTP client construction away from the current `auto` behavior. The test should cover the behavior at the narrowest practical layer, preferably around the client or connector configuration, so it remains fast and deterministic.

## Scope

This work does not add new configuration keys. It only makes the existing `forwarder_http_protocol` key effective for the Datadog forwarder HTTP client.
