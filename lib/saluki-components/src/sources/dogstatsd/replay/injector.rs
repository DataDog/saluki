use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use datadog_protos::agent::UnixDogstatsdMsg;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
#[cfg(unix)]
use saluki_io::net::{unix::unixgram_sendmsg_with_credentials, ProcessCredentials};
#[cfg(unix)]
use tokio::net::UnixDatagram;
use tokio::time::Instant;

use super::{file::MIN_NANO_VERSION, reader::TrafficCaptureReader, REPLAY_CREDENTIALS_GID};

/// Sends replay packets into the configured DogStatsD Unix datagram socket.
///
/// This injector mirrors the Go replay path closely:
/// it reuses captured timestamps to preserve inter-packet cadence,
/// sends only the valid payload prefix for each record,
/// and attaches synthetic replay credentials so ADP can recover the captured PID during enrichment.
pub struct DogStatsDReplayInjector {
    socket_path: PathBuf,
    #[cfg(unix)]
    socket: UnixDatagram,
}

impl DogStatsDReplayInjector {
    /// Creates a new replay injector targeting the given DogStatsD Unix datagram socket.
    ///
    /// # Errors
    ///
    /// If the replay socket cannot be created or connected, or if the current platform does not support Unix domain
    /// sockets, an error is returned.
    pub async fn from_socket_path(path: impl AsRef<Path>) -> Result<Self, GenericError> {
        let socket_path = path.as_ref().to_path_buf();

        #[cfg(unix)]
        {
            let socket = UnixDatagram::unbound().error_context("Failed to create replay Unix datagram socket.")?;
            socket.connect(&socket_path).with_error_context(|| {
                format!(
                    "Failed to connect replay injector to DogStatsD Unix socket '{}'.",
                    socket_path.display()
                )
            })?;

            Ok(Self { socket_path, socket })
        }

        #[cfg(not(unix))]
        {
            let _ = socket_path;
            Err(generic_error!(
                "DogStatsD replay requires Unix domain sockets, which are not supported on this platform."
            ))
        }
    }

    /// Replays a single pass over the given capture file reader.
    ///
    /// The reader is rewound to the first packet before replay starts, so the same reader can be used for multiple
    /// replay loops.
    ///
    /// # Errors
    ///
    /// If a capture record is malformed, if synthetic replay credentials cannot be built, or if a replay packet cannot
    /// be sent to the DogStatsD socket, an error is returned.
    pub async fn replay_once(&self, reader: &mut TrafficCaptureReader, verbose: bool) -> Result<(), GenericError> {
        reader.rewind();

        let mut first_timestamp = None;
        let replay_start = Instant::now();

        while let Some(message) = reader.read_next()? {
            if let Some(replay_offset) = replay_offset(reader.version(), first_timestamp, message.timestamp) {
                let elapsed = replay_start.elapsed();
                if replay_offset > elapsed {
                    tokio::time::sleep(replay_offset - elapsed).await;
                }
            } else {
                first_timestamp = Some(message.timestamp);
            }

            let payload = replay_payload(&message)?;
            let process_credentials = replay_process_credentials(message.pid)?;

            #[cfg(unix)]
            {
                let bytes_sent = unixgram_sendmsg_with_credentials(&self.socket, payload, process_credentials)
                    .await
                    .with_error_context(|| {
                        format!(
                            "Failed to send replay packet to DogStatsD Unix socket '{}'.",
                            self.socket_path.display()
                        )
                    })?;
                if verbose {
                    println!("Sent payload: {bytes_sent} bytes");
                }
            }

            #[cfg(not(unix))]
            {
                let _ = payload;
                let _ = process_credentials;
                let _ = verbose;
                return Err(generic_error!(
                    "DogStatsD replay requires Unix domain sockets, which are not supported on this platform."
                ));
            }
        }

        Ok(())
    }
}

fn replay_payload(message: &UnixDogstatsdMsg) -> Result<&[u8], GenericError> {
    let payload_size = usize::try_from(message.payload_size)
        .map_err(|_| generic_error!("Replay packet contained a negative payload size."))?;
    if payload_size > message.payload.len() {
        return Err(generic_error!(
            "Replay packet payload size {} exceeded payload buffer length {}.",
            payload_size,
            message.payload.len()
        ));
    }

    Ok(&message.payload[..payload_size])
}

#[cfg(unix)]
fn replay_process_credentials(captured_pid: i32) -> Result<ProcessCredentials, GenericError> {
    let captured_pid =
        u32::try_from(captured_pid).map_err(|_| generic_error!("Replay packet contained a negative captured PID."))?;
    let replay_pid = i32::try_from(std::process::id())
        .map_err(|_| generic_error!("Current process ID did not fit in the replay credential format."))?;

    Ok(ProcessCredentials {
        pid: replay_pid,
        uid: captured_pid,
        gid: REPLAY_CREDENTIALS_GID,
    })
}

#[cfg(not(unix))]
fn replay_process_credentials(_captured_pid: i32) -> Result<(), GenericError> {
    Err(generic_error!(
        "DogStatsD replay requires Unix domain sockets, which are not supported on this platform."
    ))
}

fn replay_offset(version: u8, first_timestamp: Option<i64>, current_timestamp: i64) -> Option<Duration> {
    let first_timestamp = first_timestamp?;
    let delta = current_timestamp.saturating_sub(first_timestamp) as u64;

    Some(if version < MIN_NANO_VERSION {
        Duration::from_secs(delta)
    } else {
        Duration::from_nanos(delta)
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use datadog_protos::agent::UnixDogstatsdMsg;

    use super::{replay_offset, replay_payload, replay_process_credentials};
    use crate::sources::dogstatsd::replay::REPLAY_CREDENTIALS_GID;

    #[test]
    fn replay_payload_uses_payload_size_prefix() {
        let message = UnixDogstatsdMsg {
            payload_size: 4,
            payload: b"metric-padding".to_vec(),
            ..Default::default()
        };

        assert_eq!(replay_payload(&message).expect("payload prefix should parse"), b"metr");
    }

    #[test]
    fn replay_offset_uses_nanoseconds_for_v3_files() {
        let offset = replay_offset(3, Some(10), 35).expect("offset should exist");

        assert_eq!(offset, Duration::from_nanos(25));
    }

    #[test]
    fn replay_offset_uses_seconds_for_v2_files() {
        let offset = replay_offset(2, Some(10), 12).expect("offset should exist");

        assert_eq!(offset, Duration::from_secs(2));
    }

    #[test]
    fn replay_offset_is_absent_for_first_packet() {
        assert!(replay_offset(3, None, 10).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn replay_process_credentials_use_captured_pid_in_uid() {
        let credentials = replay_process_credentials(42).expect("credentials should build");

        assert_eq!(credentials.uid, 42);
        assert_eq!(credentials.gid, REPLAY_CREDENTIALS_GID);
        assert_eq!(credentials.pid, std::process::id() as i32);
    }
}
