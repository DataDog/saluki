use crate::invalid;
use std::io;

pub(crate) const SETUP_VERSION: u32 = 1;
pub(crate) const LAYOUT_VERSION: u32 = 3;
pub(crate) const LAYOUT_ID: &[u8; 8] = b"MQUEUE03";
pub(crate) const HEADER_MAGIC: &[u8; 8] = b"MCHKSHM3";
pub(crate) const HEADER_SIZE: usize = 64;
pub(crate) const RECORD_HEADER_SIZE: u32 = 8;

// Broadcast (single publisher, many subscribers) transport identity. It is
// deliberately distinct from the SPSC identity so a peer cannot silently treat
// a broadcast mapping as an SPSC queue. The layout version started at 1.
pub(crate) const BROADCAST_LAYOUT_VERSION: u32 = 1;
pub(crate) const BROADCAST_LAYOUT_ID: &[u8; 8] = b"MQUEUEBC";
pub(crate) const BROADCAST_HEADER_MAGIC: &[u8; 8] = b"MBRDSHM1";

/// Fixed byte layout of the broadcast control region. These offsets are part
/// of the wire contract; no compiler-selected struct layout is ever mapped.
pub(crate) const BROADCAST_WRITE_OFFSET: usize = 128;
pub(crate) const BROADCAST_SLOTS_OFFSET: usize = 256;
pub(crate) const BROADCAST_SLOT_STRIDE: usize = 128;

// Field offsets inside immutable broadcast metadata (offset 0).
pub(crate) const BROADCAST_META_MAGIC: usize = 0;
pub(crate) const BROADCAST_META_LAYOUT_VERSION: usize = 8;
pub(crate) const BROADCAST_META_PROTOCOL_VERSION: usize = 12;
pub(crate) const BROADCAST_META_SESSION: usize = 16;
pub(crate) const BROADCAST_META_REGION_SIZE: usize = 24;
pub(crate) const BROADCAST_META_RING_OFFSET: usize = 28;
pub(crate) const BROADCAST_META_CAPACITY: usize = 32;
pub(crate) const BROADCAST_META_RECORD_HEADER: usize = 36;
pub(crate) const BROADCAST_META_SLOT_STRIDE: usize = 40;
pub(crate) const BROADCAST_META_MAX_SUBSCRIBERS: usize = 44;
pub(crate) const BROADCAST_META_SLOTS_OFFSET: usize = 48;

// Field offsets inside one subscriber slot (relative to its stride base).
pub(crate) const BROADCAST_SLOT_READ_CURSOR: usize = 0;
pub(crate) const BROADCAST_SLOT_STATE: usize = 4;
pub(crate) const BROADCAST_SLOT_GENERATION: usize = 8;
pub(crate) const BROADCAST_SLOT_PRODUCER_WAITING: usize = 12;

// Subscriber slot lifecycle states stored in BROADCAST_SLOT_STATE.
pub(crate) const BROADCAST_SLOT_FREE: u32 = 0;
pub(crate) const BROADCAST_SLOT_RESERVED: u32 = 1;
pub(crate) const BROADCAST_SLOT_ACTIVE: u32 = 2;
pub(crate) const BROADCAST_SLOT_RETIRING: u32 = 3;

/// Application identity and record types exchanged during setup.
#[derive(Debug, Clone, Copy)]
pub struct ProtocolDescriptor {
    pub id: [u8; 8],
    pub version: u32,
    pub message_types: &'static [u32],
}
impl ProtocolDescriptor {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.message_types.is_empty() || self.message_types.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "message type IDs must be nonzero and nonempty",
            ));
        }
        for (index, id) in self.message_types.iter().enumerate() {
            if self.message_types[..index].contains(id) {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "duplicate message type ID"));
            }
        }
        Ok(())
    }
    pub(crate) fn supports(&self, kind: u32) -> bool {
        self.message_types.contains(&kind)
    }
}

pub(crate) fn contract(role: u8, protocol: &ProtocolDescriptor) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(29);
    bytes.extend_from_slice(&SETUP_VERSION.to_be_bytes());
    bytes.extend_from_slice(&protocol.version.to_be_bytes());
    bytes.extend_from_slice(&LAYOUT_VERSION.to_be_bytes());
    bytes.push(role);
    bytes.extend_from_slice(&protocol.id);
    bytes.extend_from_slice(LAYOUT_ID);
    bytes
}

/// Builds the broadcast compatibility tuple. It matches the SPSC tuple shape
/// but carries the broadcast layout identity and version.
pub(crate) fn broadcast_contract(role: u8, protocol: &ProtocolDescriptor) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(29);
    bytes.extend_from_slice(&SETUP_VERSION.to_be_bytes());
    bytes.extend_from_slice(&protocol.version.to_be_bytes());
    bytes.extend_from_slice(&BROADCAST_LAYOUT_VERSION.to_be_bytes());
    bytes.push(role);
    bytes.extend_from_slice(&protocol.id);
    bytes.extend_from_slice(BROADCAST_LAYOUT_ID);
    bytes
}

pub(crate) fn check_broadcast_contract(bytes: &[u8], role: u8, protocol: &ProtocolDescriptor) -> io::Result<()> {
    let expected = broadcast_contract(role, protocol);
    if bytes.len() != expected.len() {
        return Err(invalid(format!(
            "broadcast contract length: expected {}, received {}",
            expected.len(),
            bytes.len()
        )));
    }
    if bytes != expected {
        return Err(invalid(format!(
            "broadcast contract mismatch: expected {}, received {}",
            hex(&expected),
            hex(bytes)
        )));
    }
    Ok(())
}

pub(crate) fn check_contract(bytes: &[u8], role: u8, protocol: &ProtocolDescriptor) -> io::Result<()> {
    let expected = contract(role, protocol);
    if bytes.len() != expected.len() {
        return Err(invalid(format!(
            "contract length: expected {}, received {}",
            expected.len(),
            bytes.len()
        )));
    }
    if bytes != expected {
        return Err(invalid(format!(
            "contract mismatch: expected {}, received {}",
            hex(&expected),
            hex(bytes)
        )));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_registry_validation() {
        for kinds in [&[][..], &[0][..], &[1, 1][..]] {
            let descriptor = ProtocolDescriptor {
                id: *b"TEST0001",
                version: 1,
                message_types: kinds,
            };
            assert_eq!(descriptor.validate().unwrap_err().kind(), io::ErrorKind::InvalidInput);
        }
        let descriptor = ProtocolDescriptor {
            id: *b"TEST0001",
            version: 1,
            message_types: &[1, 42],
        };
        descriptor.validate().unwrap();
        assert!(descriptor.supports(42));
    }
}
