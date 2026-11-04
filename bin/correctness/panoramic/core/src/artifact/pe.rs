//! Reads the machine architecture of PE executables.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

/// Reads the DOS header and PE file header; `path` labels errors.
pub(super) fn machine(file: &mut File, path: &Path) -> Result<u16, String> {
    let invalid = || format!("'{}' is not a PE executable", path.display());
    file.rewind()
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    let mut dos = [0; 64];
    file.read_exact(&mut dos).map_err(|_| invalid())?;
    if &dos[..2] != b"MZ" {
        return Err(invalid());
    }
    let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap());
    if offset < dos.len() as u32 {
        return Err(invalid());
    }
    file.seek(SeekFrom::Start(u64::from(offset))).map_err(|_| invalid())?;
    let mut header = [0; 24];
    file.read_exact(&mut header).map_err(|_| invalid())?;
    let characteristics = u16::from_le_bytes(header[22..24].try_into().unwrap());
    if &header[..4] != b"PE\0\0" || characteristics & 0x0002 == 0 {
        return Err(invalid());
    }
    Ok(u16::from_le_bytes(header[4..6].try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::test_util::write_pe;

    #[test]
    fn pe_executables_report_their_machine_at_the_declared_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.exe");
        for expected in [0x8664, 0xAA64, 0x014c] {
            write_pe(&path, expected);
            let mut file = File::open(&path).unwrap();
            file.seek(SeekFrom::End(0)).unwrap();
            assert_eq!(machine(&mut file, &path), Ok(expected));
        }
    }

    #[test]
    fn non_pe_truncated_and_non_executable_headers_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.exe");
        type Mutation = fn(&mut Vec<u8>);
        let mutations: &[Mutation] = &[
            |bytes| bytes.clear(),
            |bytes| bytes[0] = 0,
            |bytes| bytes.truncate(63),
            |bytes| bytes[60..64].copy_from_slice(&u32::MAX.to_le_bytes()),
            |bytes| bytes[60..64].copy_from_slice(&0u32.to_le_bytes()),
            |bytes| bytes[128] = 0,
            |bytes| bytes.truncate(151),
            |bytes| bytes[150..152].copy_from_slice(&0x2000u16.to_le_bytes()),
        ];
        for mutate in mutations {
            write_pe(&path, 0x8664);
            let mut bytes = std::fs::read(&path).unwrap();
            mutate(&mut bytes);
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(
                machine(&mut File::open(&path).unwrap(), &path),
                Err(format!("'{}' is not a PE executable", path.display()))
            );
        }
    }
}
