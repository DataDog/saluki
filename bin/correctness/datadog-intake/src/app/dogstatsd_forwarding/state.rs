use std::sync::{Arc, Mutex};

use base64::{engine::general_purpose::STANDARD, Engine as _};

#[derive(Clone)]
pub struct DogStatsDForwardingState {
    packets: Arc<Mutex<Vec<String>>>,
}

impl DogStatsDForwardingState {
    pub fn new() -> Self {
        Self {
            packets: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn record_packet(&self, packet: &[u8]) {
        let packet = STANDARD.encode(packet);
        self.packets.lock().unwrap().push(packet);
    }

    pub fn dump_packets(&self) -> Vec<String> {
        self.packets.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::DogStatsDForwardingState;

    #[test]
    fn state_records_exact_packet_bytes_as_base64() {
        let state = DogStatsDForwardingState::new();

        state.record_packet(b"metric:1|c");
        state.record_packet(&[0, 159, 146, 150, 255]);

        assert_eq!(state.dump_packets(), ["bWV0cmljOjF8Yw==", "AJ+Slv8="]);
    }
}
