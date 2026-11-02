//! Fixtures for artifact verification tests.

use std::path::Path;

/// Writes `b"tool"`, creating parent directories and setting Unix execute permissions.
///
/// # Panics
///
/// Panics if `path` has no parent or a filesystem operation fails.
pub fn write_executable(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"tool").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// Writes an ELF header with a load or dynamic segment and an optional interpreter segment.
#[cfg(test)]
pub(crate) fn write_elf(path: &Path, class: u8, endian: u8, kind: u16, machine: u16, interpreter: bool) {
    let count = if interpreter { 2u16 } else { 1 };
    let mut bytes = vec![0; 64 + usize::from(count) * 56];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = class;
    bytes[5] = endian;
    bytes[6] = 1;
    bytes[16..18].copy_from_slice(&kind.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&count.to_le_bytes());
    let segment = if kind == 3 { 2u32 } else { 1 };
    bytes[64..68].copy_from_slice(&segment.to_le_bytes());
    if interpreter {
        bytes[120..124].copy_from_slice(&3u32.to_le_bytes());
    }
    write_executable(path);
    std::fs::write(path, bytes).unwrap();
}
