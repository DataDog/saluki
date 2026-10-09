# Saluki FIT transport

`saluki-fit` provides single-producer, single-consumer shared-memory IPC. It owns setup, mappings, a bounded message ring, and native address waits. Application message types and codecs belong in a separate protocol crate.

This crate is adapted from the Fast IPC Toolkit's `lib/rust/fit-core` template at commit `3341fc3348ccd2193a071499b948162d78b539f5`. Its imported setup framing, shared-memory layout, and ring rules are kept together here so the ACR and ADP integrations can use the same transport. The Rust source differs from the template only in formatting and the crate-level target gate.

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

Run `MACOSX_DEPLOYMENT_TARGET=14.4 cargo test -p saluki-fit` on macOS to test the crate. The original toolkit also contains process-level examples; the Checks integration adds its own protocol and process tests in later commits.
