//! Ack-only stateful metrics intake for load tests.
//!
//! Acknowledges every `StatefulStream` batch without decompressing or decoding it, so the intake adds as
//! little CPU and memory as possible next to ADP. It doesn't validate payloads; use the Foldspace
//! reference intake for that.
//!
//! Configured through environment variables:
//!
//! - `LISTEN_ADDR`: listen address (default `127.0.0.1:9201`).
//! - `ACK_DELAY_MS`: latency added to every acknowledgement, as from a distant intake.
//! - `ACK_PAUSE_EVERY_SECS` and `ACK_PAUSE_FOR_SECS`: withhold acknowledgements for the last
//!   `ACK_PAUSE_FOR_SECS` of every period, as from a stalled intake that stays connected.
//! - `OUTAGE_EVERY_SECS` and `OUTAGE_FOR_SECS`: for the last `OUTAGE_FOR_SECS` of every period, fail
//!   open streams and reject new ones with `UNAVAILABLE`.
//!
//! Windows sit at the end of each period, so every run starts with a healthy warm-up.

use std::{env, error::Error, net::SocketAddr, time::Duration};

use foldspace_core::proto::stateful::{
    batch_status,
    stateful_intake_server::{StatefulIntake, StatefulIntakeServer},
    BatchStatus, StatefulBatch, StatelessRequest, StatelessResponse,
};
use futures::{
    future::pending,
    stream::{self, BoxStream},
    StreamExt as _,
};
use tokio::{
    select,
    sync::mpsc,
    time::{sleep_until, Instant},
};
use tonic::{transport::Server, Request, Response, Status, Streaming};

/// A window of `length` at the end of every `every`, measured from process start.
#[derive(Clone, Copy)]
struct Window {
    every: Duration,
    length: Duration,
}

impl Window {
    fn from_env(every: &str, length: &str) -> Result<Option<Self>, Box<dyn Error>> {
        match (
            env_duration(every, Duration::from_secs)?,
            env_duration(length, Duration::from_secs)?,
        ) {
            (None, None) => Ok(None),
            (Some(every), Some(length)) if !length.is_zero() && length < every => Ok(Some(Self { every, length })),
            _ => Err(format!("{every} and {length} must both be set, with {length} nonzero and below {every}").into()),
        }
    }

    fn phase(&self, started: Instant, at: Instant) -> Duration {
        let elapsed = at.saturating_duration_since(started).as_nanos();
        Duration::from_nanos((elapsed % self.every.as_nanos()) as u64)
    }

    fn is_active(&self, started: Instant, at: Instant) -> bool {
        self.phase(started, at) >= self.every - self.length
    }

    /// When the next window starts: `at` itself if one is active.
    fn next_start(&self, started: Instant, at: Instant) -> Instant {
        let phase = self.phase(started, at);
        at + (self.every - self.length).saturating_sub(phase)
    }

    /// When the window containing `at` ends: `at` itself if none is active.
    fn end(&self, started: Instant, at: Instant) -> Instant {
        if self.is_active(started, at) {
            at + (self.every - self.phase(started, at))
        } else {
            at
        }
    }
}

#[derive(Clone, Copy)]
struct AckIntake {
    started: Instant,
    ack_delay: Duration,
    ack_pause: Option<Window>,
    outage: Option<Window>,
}

impl AckIntake {
    fn ack_due(&self, arrived: Instant) -> Instant {
        let due = arrived + self.ack_delay;
        self.ack_pause.map_or(due, |pause| pause.end(self.started, due))
    }

    fn next_outage(&self) -> Option<Instant> {
        self.outage
            .map(|outage| outage.next_start(self.started, Instant::now()))
    }
}

#[tonic::async_trait]
impl StatefulIntake for AckIntake {
    type StatefulStreamStream = BoxStream<'static, Result<BatchStatus, Status>>;

    async fn stateful_stream(
        &self, request: Request<Streaming<StatefulBatch>>,
    ) -> Result<Response<Self::StatefulStreamStream>, Status> {
        if self
            .outage
            .is_some_and(|outage| outage.is_active(self.started, Instant::now()))
        {
            return Err(Status::unavailable("simulated intake outage"));
        }
        // Read on a separate task so each batch is timestamped on arrival, not when its turn to be acknowledged comes.
        let mut inbound = request.into_inner();
        let (arrivals, pending_acks) = mpsc::unbounded_channel();
        let intake = *self;
        tokio::spawn(async move {
            loop {
                let arrival = match inbound.message().await {
                    Ok(Some(batch)) => Ok((batch.batch_id, intake.ack_due(Instant::now()))),
                    Ok(None) => break,
                    Err(status) => Err(status),
                };
                let failed = arrival.is_err();
                if arrivals.send(arrival).is_err() || failed {
                    break;
                }
            }
        });
        // Each step yields the next acknowledgement, or ends the stream with an error when an outage starts first.
        let acks = stream::unfold(Some(pending_acks), move |pending_acks| async move {
            let mut pending_acks = pending_acks?;
            let outage = || Some((Err(Status::unavailable("simulated intake outage")), None));
            let (batch_id, due) = select! {
                arrival = pending_acks.recv() => match arrival? {
                    Ok(arrival) => arrival,
                    Err(status) => return Some((Err(status), None)),
                },
                _ = wait_until(intake.next_outage()) => return outage(),
            };
            select! {
                _ = sleep_until(due) => {},
                _ = wait_until(intake.next_outage()) => return outage(),
            }
            let ack = BatchStatus {
                batch_id,
                status: batch_status::Status::Ok.into(),
            };
            Some((Ok(ack), Some(pending_acks)))
        });
        Ok(Response::new(acks.boxed()))
    }

    async fn stateless(&self, _: Request<StatelessRequest>) -> Result<Response<StatelessResponse>, Status> {
        Err(Status::unimplemented("stateless delivery is not supported"))
    }
}

async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}

fn env_duration(name: &str, unit: fn(u64) -> Duration) -> Result<Option<Duration>, Box<dyn Error>> {
    match env::var(name) {
        Ok(value) => Ok(Some(unit(
            value.parse().map_err(|error| format!("{name}={value}: {error}"))?,
        ))),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(format!("{name}: {error}").into()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    let addr: SocketAddr = env::var("LISTEN_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:9201".to_string())
        .parse()?;
    let intake = AckIntake {
        started: Instant::now(),
        ack_delay: env_duration("ACK_DELAY_MS", Duration::from_millis)?.unwrap_or_default(),
        ack_pause: Window::from_env("ACK_PAUSE_EVERY_SECS", "ACK_PAUSE_FOR_SECS")?,
        outage: Window::from_env("OUTAGE_EVERY_SECS", "OUTAGE_FOR_SECS")?,
    };
    Server::builder()
        .add_service(StatefulIntakeServer::new(intake).max_decoding_message_size(usize::MAX))
        .serve(addr)
        .await?;
    Ok(())
}
