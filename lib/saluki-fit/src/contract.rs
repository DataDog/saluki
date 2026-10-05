use crate::invalid;
use std::io;

pub(crate) const SETUP_VERSION: u32 = 1;
pub(crate) const LAYOUT_VERSION: u32 = 3;
pub(crate) const LAYOUT_ID: &[u8; 8] = b"MQUEUE03";
pub(crate) const HEADER_MAGIC: &[u8; 8] = b"MCHKSHM3";
pub(crate) const HEADER_SIZE: usize = 64;
pub(crate) const RECORD_HEADER_SIZE: u32 = 8;

/// Application identity and record types exchanged during setup.
#[derive(Debug, Clone, Copy)]
pub struct ProtocolDescriptor {
    pub id: [u8; 8],
    pub version: u32,
    pub message_types: &'static [u32],
}
impl ProtocolDescriptor {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.message_types.is_empty() || self.message_types.iter().any(|&id| id == 0) {
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
