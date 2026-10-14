# Checks FIT protocol

This crate owns the application contract shared by the Agent Check Runner producer and Agent Data Plane consumer. It translates the field meanings in the existing `lib/protos/datadog/proto/checks/v1/*.proto` definitions into owned Rust types and the custom byte layout in [the wire contract](protocol/checks-fit.md). No protobuf serializer is used for FIT payloads.

`Producer` and `Consumer` wrap `saluki-fit` with one application identity and version. `Producer::send_batch` stages at most one ring's usable capacity, publishes the fitting ordered prefix, and reports queue rejection, encoding failure, and post-publication notification failure separately. `Consumer::receive` returns a decoded `Message`; malformed payloads error after core has reclaimed their bytes. The application decides how to report or continue after such an error. The typed handles also offer local setup and receive cancellation.

This crate does not depend on Saluki event types or ACR event types. Each application converts its own events at its boundary. Unknown enum numbers are preserved as `i32` values so ADP can keep its existing semantic rejection behavior.

The transport supports Linux and macOS on little-endian x86-64 or AArch64; macOS shared address waits require 14.4 or newer. The ring defaults to 1 MiB, and all payloads must fit the negotiated ring capacity, even when below the absolute 1 GiB layout bound.
