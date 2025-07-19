FROM rust:1.87-alpine3.22 AS builder

WORKDIR /build
COPY . /build

RUN rustup component add rust-src --toolchain nightly-x86_64-unknown-linux-musl
RUN cargo +nightly build --release -Z build-std=std,panic_abort -Z build-std-features=panic_immediate_abort
RUN mv target/release/subagent-supervisor /subagent-supervisor
RUN strip --strip-unneeded --strip-debug /subagent-supervisor

FROM alpine:3.22
COPY --from=builder /subagent-supervisor /subagent-supervisor

ENTRYPOINT ["/subagent-supervisor"]
