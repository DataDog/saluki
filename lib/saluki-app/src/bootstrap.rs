//! Bootstrap utilities.

use std::{io, path::Path};

/// Writes the current process ID to the specified file.
///
/// # Errors
///
/// If the PID cannot be written to the file, an error is returned.
pub fn update_pid_file<P: AsRef<Path>>(pid_file: P) -> io::Result<()> {
    let pid_string = std::process::id().to_string();

    std::fs::write(pid_file, pid_string)
}
