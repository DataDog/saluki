//! Reads ELF headers to check static container executables and their machine architecture.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

/// A CPU architecture a container binary can be verified for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Architecture {
    X86_64,
    Aarch64,
}

impl Architecture {
    /// Returns the architecture's display name.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// Returns the ELF `e_machine` value for this architecture.
    pub(crate) fn elf_machine(self) -> u16 {
        match self {
            Self::X86_64 => 62,
            Self::Aarch64 => 183,
        }
    }

    /// Returns the supported architecture with this ELF `e_machine` value.
    pub(crate) fn from_elf_machine(machine: u16) -> Option<Self> {
        [Self::X86_64, Self::Aarch64]
            .into_iter()
            .find(|architecture| architecture.elf_machine() == machine)
    }

    /// Returns the PE machine value for this architecture.
    pub(crate) fn pe_machine(self) -> u16 {
        match self {
            Self::X86_64 => 0x8664,
            Self::Aarch64 => 0xAA64,
        }
    }

    /// Returns the supported architecture with this PE machine value.
    pub(crate) fn from_pe_machine(machine: u16) -> Option<Self> {
        [Self::X86_64, Self::Aarch64]
            .into_iter()
            .find(|architecture| architecture.pe_machine() == machine)
    }
}

/// Returns the ELF machine of a 64-bit little-endian executable without a `PT_INTERP` segment.
///
/// Reads `file` from the start; `path` labels errors. Accepts `ET_DYN` because static-pie executables use it;
/// shared libraries with the same header properties also pass these checks.
///
/// # Errors
///
/// File inspection, seeking or reading can fail. ELF checks report one of these errors:
/// - `'<path>' is not an ELF executable`
/// - `'<path>' is not a 64-bit little-endian ELF executable`
/// - `'<path>' has a truncated ELF program header table`
/// - `'<path>' is dynamically linked; containers need a static executable`
pub(super) fn static_machine(file: &mut File, path: &Path) -> Result<u16, String> {
    let (machine, has_interpreter) = read_header(file, path)?;
    if has_interpreter {
        return Err(format!(
            "'{}' is dynamically linked; containers need a static executable",
            path.display()
        ));
    }
    Ok(machine)
}

fn read_header(file: &mut File, path: &Path) -> Result<(u16, bool), String> {
    let invalid = || format!("'{}' is not an ELF executable", path.display());
    let truncated = || format!("'{}' has a truncated ELF program header table", path.display());
    file.rewind()
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    let length = file
        .metadata()
        .map_err(|error| format!("cannot inspect '{}': {error}", path.display()))?
        .len();
    let mut header = [0; 64];
    file.read_exact(&mut header).map_err(|_| invalid())?;
    if &header[..4] != b"\x7fELF" {
        return Err(invalid());
    }
    if header[4] != 2 || header[5] != 1 {
        return Err(format!(
            "'{}' is not a 64-bit little-endian ELF executable",
            path.display()
        ));
    }
    let kind = u16::from_le_bytes(header[16..18].try_into().unwrap());
    if !matches!(kind, 2 | 3) {
        return Err(invalid());
    }
    let machine = u16::from_le_bytes(header[18..20].try_into().unwrap());
    let offset = u64::from_le_bytes(header[32..40].try_into().unwrap());
    let entry_size = u64::from(u16::from_le_bytes(header[54..56].try_into().unwrap()));
    let count = u64::from(u16::from_le_bytes(header[56..58].try_into().unwrap()));
    if count == 0 {
        return Err(invalid());
    }
    if entry_size < 56 || offset.checked_add(entry_size * count).is_none_or(|end| end > length) {
        return Err(truncated());
    }
    let mut has_interpreter = false;
    // Read only each entry's fixed header; the declared stride may include extra bytes.
    for index in 0..count {
        file.seek(SeekFrom::Start(offset + index * entry_size))
            .map_err(|_| truncated())?;
        let mut entry = [0; 56];
        file.read_exact(&mut entry).map_err(|_| truncated())?;
        has_interpreter |= u32::from_le_bytes(entry[..4].try_into().unwrap()) == 3;
    }
    Ok((machine, has_interpreter))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifact::test_util::write_elf;

    #[test]
    fn a_static_executable_reports_its_machine_and_no_interpreter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        for (kind, machine) in [(3, 62), (2, 183)] {
            write_elf(&path, 2, 1, kind, machine, false);
            assert_eq!(
                read_header(&mut File::open(&path).unwrap(), &path),
                Ok((machine, false))
            );
            assert_eq!(static_machine(&mut File::open(&path).unwrap(), &path), Ok(machine));
        }
    }

    #[test]
    fn an_executable_with_an_interpreter_is_reported_as_dynamically_linked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        write_elf(&path, 2, 1, 3, 62, true);
        assert_eq!(read_header(&mut File::open(&path).unwrap(), &path), Ok((62, true)));
        assert_eq!(
            static_machine(&mut File::open(&path).unwrap(), &path),
            Err(format!(
                "'{}' is dynamically linked; containers need a static executable",
                path.display()
            ))
        );
    }

    #[test]
    fn files_that_are_not_64_bit_little_endian_elf_executables_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        type Mutation = fn(&mut Vec<u8>);
        let cases: &[(&str, Mutation, &str)] = &[
            ("empty", |bytes| bytes.clear(), "is not an ELF executable"),
            (
                "script",
                |bytes| *bytes = b"#!/bin/sh\nexit 0\n".to_vec(),
                "is not an ELF executable",
            ),
            ("magic", |bytes| bytes[0] = 0, "is not an ELF executable"),
            ("short-header", |bytes| bytes.truncate(63), "is not an ELF executable"),
            (
                "32-bit",
                |bytes| bytes[4] = 1,
                "is not a 64-bit little-endian ELF executable",
            ),
            (
                "big-endian",
                |bytes| bytes[5] = 2,
                "is not a 64-bit little-endian ELF executable",
            ),
            (
                "relocatable",
                |bytes| bytes[16..18].copy_from_slice(&1u16.to_le_bytes()),
                "is not an ELF executable",
            ),
            (
                "core",
                |bytes| bytes[16..18].copy_from_slice(&4u16.to_le_bytes()),
                "is not an ELF executable",
            ),
            (
                "no-program-headers",
                |bytes| bytes[56..58].copy_from_slice(&0u16.to_le_bytes()),
                "is not an ELF executable",
            ),
            (
                "past-end",
                |bytes| bytes[32..40].copy_from_slice(&4096u64.to_le_bytes()),
                "has a truncated ELF program header table",
            ),
            (
                "short-entry",
                |bytes| bytes[54..56].copy_from_slice(&55u16.to_le_bytes()),
                "has a truncated ELF program header table",
            ),
            (
                "short-table",
                |bytes| {
                    bytes.pop();
                },
                "has a truncated ELF program header table",
            ),
            (
                "overflow",
                |bytes| bytes[32..40].copy_from_slice(&u64::MAX.to_le_bytes()),
                "has a truncated ELF program header table",
            ),
        ];
        for (name, mutate, problem) in cases {
            write_elf(&path, 2, 1, 2, 62, false);
            let mut bytes = std::fs::read(&path).unwrap();
            mutate(&mut bytes);
            std::fs::write(&path, bytes).unwrap();
            assert_eq!(
                read_header(&mut File::open(&path).unwrap(), &path),
                Err(format!("'{}' {problem}", path.display())),
                "{name}"
            );
            assert_eq!(
                static_machine(&mut File::open(&path).unwrap(), &path),
                Err(format!("'{}' {problem}", path.display())),
                "{name}"
            );
        }
    }

    #[test]
    fn program_headers_use_the_declared_offset_stride_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        write_elf(&path, 2, 1, 3, 62, true);
        let original = std::fs::read(&path).unwrap();
        let mut bytes = vec![0; 128 + 2 * 64];
        bytes[..64].copy_from_slice(&original[..64]);
        bytes[32..40].copy_from_slice(&128u64.to_le_bytes());
        bytes[54..56].copy_from_slice(&64u16.to_le_bytes());
        bytes[128..184].copy_from_slice(&original[64..120]);
        bytes[192..248].copy_from_slice(&original[120..176]);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_header(&mut File::open(&path).unwrap(), &path), Ok((62, true)));
        bytes[56..58].copy_from_slice(&1u16.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(read_header(&mut File::open(&path).unwrap(), &path), Ok((62, false)));
    }

    #[test]
    fn architectures_name_themselves_and_their_elf_machine() {
        for (architecture, name, machine) in [
            (Architecture::X86_64, "x86_64", 62),
            (Architecture::Aarch64, "aarch64", 183),
        ] {
            assert_eq!(architecture.name(), name);
            assert_eq!(architecture.elf_machine(), machine);
            assert_eq!(Architecture::from_elf_machine(machine), Some(architecture));
        }
        assert_eq!(Architecture::from_elf_machine(243), None);
    }
}
