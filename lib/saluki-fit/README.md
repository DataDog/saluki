# Saluki FIT transport

`saluki-fit` provides shared-memory IPC in two modes: single-producer/single-consumer rings, and a single-publisher broadcast ring with dynamic subscribers. It owns setup, mappings, bounded message rings, and native address waits. Application message types and codecs belong in a separate protocol crate.

This crate is adapted from the Fast IPC Toolkit's `lib/rust/fit-core` template at commit `788233d` (broadcast transport, on top of `3341fc3348ccd2193a071499b948162d78b539f5` plus the intervening fixes). Setup framing, shared-memory layouts, and ring rules are kept together here so the ACR and ADP integrations use the same transport. The Rust source differs from the template in two deliberate ways: the crate-level target gate replaces the template's `compile_error!` so unrelated Saluki targets still build, and `Producer::ring_capacity` exposes the capacity that the Checks protocol needs to reject oversized records before encoding.

## Supported systems

The transport supports little-endian x86-64 and AArch64 on Linux or macOS. Shared address waits require macOS 14.4 or newer; build macOS binaries using `MACOSX_DEPLOYMENT_TARGET=14.4` or newer. It assumes that the peers run as the same effective user and can access the same POSIX shared-memory namespace. Other operating systems cannot use this crate.

## Setup

`Consumer::open` listens and creates a fresh shared-memory object. `Producer::connect` joins it. The peers exchange `Hello`, `Offer`, `Ready`, and `Start` over a setup-only socket; both close that socket afterward. `ConsumerConfig::new(path)` and `ProducerConfig::new(path)` use a Unix-domain socket in a private directory. Their `tcp(address)` constructors use a numeric loopback TCP address. TCP loopback does not authenticate a local user. The setup deadline defaults to 60 seconds, and the ring capacity defaults to 1 MiB. Valid capacities are multiples of eight from 16 bytes through 1 GiB.

`SetupEndpoint::parse` accepts `unix:/path`, `tcp:127.0.0.1:5101`, or a bare Unix path. The producer learns the ring capacity from the consumer's offer. The setup contract checks the application's eight-byte identity, protocol version, and supported layout version before application messages flow.

## Main IPC

`Producer::send_batch` publishes the fitting prefix and reports its accepted count, first rejection, and any notification error separately. Published records must not be retried just because waking the consumer failed. A full ring rejects incoming records immediately. `Consumer::receive` blocks when the ring is empty, copies one payload into owned bytes, and releases its ring space.

One established session has one producer and one consumer. There is no peer liveness check, automatic reconnect, or replay. If a peer exits while the other is waiting, the wait may remain blocked.

For local shutdown, pass a clone of `CancellationToken` to `Producer::connect_with_cancel`, `Consumer::open_with_cancel`, or `Consumer::receive_with_cancel`. Call `cancel` from the supervising thread, then join the worker before releasing its handle. Cancelled setup returns an `Interrupted` error; cancelled receive returns `Ok(None)`, without consuming a record. A token is permanently cancelled and supports one active receive. Cancelling an idle receive wakes the native address wait, including when cancellation races with entry into that wait. Normal idle operation still sleeps without a timeout or polling loop. Setup uses short cancellation checks within its existing 60-second deadline.

The queue record format, memory ordering, and setup framing originate from the FIT template. Changes to application codecs require an application protocol-version update. Changes to the shared queue layout require a layout-version update on both peers.

## Broadcast transport

The crate also implements the toolkit's broadcast design: one publisher writes each accepted record once into one payload ring, and every active subscriber reads it with its own cursor. This mode blocks instead of dropping when the ring is full, and subscribers may join and leave while the publisher runs.

`BroadcastPublisher::open(BroadcastPublisherConfig, ProtocolDescriptor)` creates the mapping, binds the listener, and starts a control worker. It returns without waiting for a subscriber and keeps the endpoint and shared-memory name alive for the session. `BroadcastPublisherConfig` defaults to a 1 MiB ring, 64 subscriber slots, and a per-handshake timeout, not a listener lifetime. `Subscription::subscribe(SubscriberConfig, ProtocolDescriptor)` maps the existing mapping and returns after activation, so a late subscriber receives only publications after its activation boundary.

`BroadcastPublisher::send_batch` and `send_batch_with_cancel` return `BroadcastOutcome`, which always carries the number of records already published, including on cancellation or a fatal failure. `Full` is not a normal result in this mode: the call waits for the limiting subscriber, and with no active subscriber it waits until one joins. A stopped or crashed subscriber can stall publication indefinitely. `Subscription::receive` returns `(type_id, owned_payload_bytes)` and `receive_with_cancel` returns `Ok(None)` after local cancellation. Call `Subscription::unsubscribe` for a graceful leave; a subscriber that exits without it pins its slot.

`BroadcastPublisher`, `Subscription`, `BroadcastPublisherConfig`, `SubscriberConfig`, `BroadcastOutcome`, and `DEFAULT_MAX_SUBSCRIBERS` are exported beside the SPSC API. The SPSC wire layout and behavior are unchanged; the two transports use distinct layout identities and do not interoperate.

Run `MACOSX_DEPLOYMENT_TARGET=14.4 cargo test -p saluki-fit` on macOS to test the crate. The original toolkit also contains process-level examples; the Checks integration adds its own protocol and process tests in later commits.
