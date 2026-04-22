use std::{fs, io::Cursor, path::Path};

use datadog_protos::agent::{TaggerState, UnixDogstatsdMsg};
use prost::Message;
use saluki_error::{generic_error, GenericError};

use super::file::{datadog_matcher, file_version, DATADOG_HEADER, MIN_STATE_VERSION};

/// Reads DogStatsD capture files written by the Go agent.
pub(crate) struct TrafficCaptureReader {
    contents: Vec<u8>,
    version: u8,
    offset: usize,
}

impl TrafficCaptureReader {
    /// Opens a DogStatsD capture file from disk.
    pub(crate) fn from_path(path: impl AsRef<Path>) -> Result<Self, GenericError> {
        let path = path.as_ref();
        let contents = read_capture_contents(path)?;
        let version = file_version(&contents)?;

        Ok(Self {
            contents,
            version,
            offset: DATADOG_HEADER.len(),
        })
    }

    /// Returns the parsed capture file version.
    pub(crate) fn version(&self) -> u8 {
        self.version
    }

    /// Reads the next captured packet record.
    ///
    /// Returns `Ok(None)` when the reader reaches the trailing replay-state separator.
    pub(crate) fn read_next(&mut self) -> Result<Option<UnixDogstatsdMsg>, GenericError> {
        if self.offset + 4 > self.contents.len() {
            return Ok(None);
        }

        let size = u32::from_le_bytes(
            self.contents[self.offset..self.offset + 4]
                .try_into()
                .expect("record size slice has fixed width"),
        ) as usize;
        self.offset += 4;

        // Go uses a zero-sized record to mark the start of the trailing state block.
        if size == 0 || self.offset + size > self.contents.len() {
            return Ok(None);
        }

        let record_end = self.offset + size;
        let message = UnixDogstatsdMsg::decode(&self.contents[self.offset..record_end]).map_err(|e| {
            generic_error!(
                "Failed to decode DogStatsD capture record at byte offset {}: {}",
                self.offset,
                e
            )
        })?;
        self.offset = record_end;

        Ok(Some(message))
    }

    /// Reads the trailing replay state from the end of the file.
    ///
    /// This does not modify the current record cursor.
    pub(crate) fn read_state(&self) -> Result<Option<TaggerState>, GenericError> {
        if self.version < MIN_STATE_VERSION {
            return Err(generic_error!(
                "DogStatsD capture file version {} does not contain replay state.",
                self.version
            ));
        }

        if self.contents.len() < 4 {
            return Err(generic_error!(
                "DogStatsD capture file is too short to contain replay state."
            ));
        }

        let state_size_offset = self.contents.len() - 4;
        let state_size = u32::from_le_bytes(
            self.contents[state_size_offset..]
                .try_into()
                .expect("state size slice has fixed width"),
        ) as usize;

        if state_size == 0 {
            return Ok(None);
        }

        let state_start = state_size_offset.checked_sub(state_size).ok_or_else(|| {
            generic_error!(
                "DogStatsD capture state length {} exceeds the capture file size {}.",
                state_size,
                self.contents.len()
            )
        })?;

        let state = TaggerState::decode(&self.contents[state_start..state_size_offset]).map_err(|e| {
            generic_error!(
                "Failed to decode DogStatsD replay state from byte range {}..{}: {}",
                state_start,
                state_size_offset,
                e
            )
        })?;

        Ok(Some(state))
    }
}

fn read_capture_contents(path: &Path) -> Result<Vec<u8>, GenericError> {
    let raw_contents = fs::read(path)
        .map_err(|e| generic_error!("Failed to read DogStatsD capture file '{}': {}", path.display(), e))?;
    if datadog_matcher(&raw_contents) {
        return Ok(raw_contents);
    }

    let contents = zstd::stream::decode_all(Cursor::new(raw_contents.as_slice())).map_err(|e| {
        generic_error!(
            "DogStatsD capture file '{}' was neither a raw Datadog capture nor a valid zstd-compressed capture: {}",
            path.display(),
            e
        )
    })?;
    if !datadog_matcher(&contents) {
        return Err(generic_error!(
            "DogStatsD capture file '{}' did not contain a valid Datadog capture header after zstd decompression.",
            path.display()
        ));
    }

    Ok(contents)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::PathBuf};

    use super::TrafficCaptureReader;

    const GO_CAPTURE_PAYLOAD: &[u8] = b"jaime.uds.test:8|g|#shell:test";
    const GO_CAPTURE_PID: i32 = 2809;
    const GO_CAPTURE_RECORDS: usize = 21;
    const GO_CAPTURE_PAYLOAD_BUFFER_SIZE: usize = 8192;
    const GO_CAPTURE_ANCILLARY_SIZE: i32 = 32;
    const GO_CAPTURE_CONTAINER_ID: &str =
        "container_id://c1371eaf97a11f43ac700fd8524b4ea316d83a7259282a9e9eeac8d071406b22";
    const GO_CAPTURE_STATE_PIDS: [i32; 7] = [2815, 2818, 2821, 2824, 2836, 2851, 2854];

    #[test]
    fn reads_uncompressed_go_capture_file() {
        let reader = TrafficCaptureReader::from_path(go_capture_path("datadog-capture.dog"))
            .expect("uncompressed Go capture should open");

        assert_go_capture(reader);
    }

    #[test]
    fn reads_zstd_compressed_go_capture_file() {
        let reader = TrafficCaptureReader::from_path(go_capture_path("datadog-capture.dog.zstd"))
            .expect("zstd-compressed Go capture should open");

        assert_go_capture(reader);
    }

    fn assert_go_capture(mut reader: TrafficCaptureReader) {
        assert_eq!(reader.version(), 2);

        let state = reader
            .read_state()
            .expect("Go capture state should decode")
            .expect("Go capture should include replay state");
        let state_pids = state.pid_map.keys().copied().collect::<BTreeSet<_>>();
        assert_eq!(
            state_pids,
            GO_CAPTURE_STATE_PIDS.into_iter().collect(),
            "Go capture pid map should match the vendored fixture"
        );
        assert!(
            state
                .pid_map
                .values()
                .all(|container_id| container_id == GO_CAPTURE_CONTAINER_ID),
            "Go capture pid map should point every saved pid at the same container"
        );
        assert!(state.state.contains_key(GO_CAPTURE_CONTAINER_ID));
        assert!(state.duration >= 0);

        let mut records = Vec::new();
        while let Some(record) = reader.read_next().expect("capture record should decode") {
            records.push(record);
        }

        assert_eq!(records.len(), GO_CAPTURE_RECORDS);

        let first = &records[0];
        assert_eq!(first.payload_size as usize, GO_CAPTURE_PAYLOAD.len());
        assert_eq!(&first.payload[..first.payload_size as usize], GO_CAPTURE_PAYLOAD);
        assert_eq!(first.payload.len(), GO_CAPTURE_PAYLOAD_BUFFER_SIZE);
        assert_eq!(first.pid, GO_CAPTURE_PID);
        assert_eq!(first.ancillary_size, GO_CAPTURE_ANCILLARY_SIZE);
        assert_eq!(first.ancillary.len(), GO_CAPTURE_ANCILLARY_SIZE as usize);
    }

    fn go_capture_path(file_name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/sources/dogstatsd/replay/testdata")
            .join(file_name)
    }
}
