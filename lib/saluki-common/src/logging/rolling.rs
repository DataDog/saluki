use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
};

/// A log file writer that rolls over to a new file once the current one reaches a maximum size.
///
/// Rolled files use Debian-style names: the active file is `path`, the most recently rolled file is `path.1`, and the
/// oldest is `path.N`, where `N` is the maximum number of rolled files to keep. Rolling over deletes `path.N`, renames
/// each remaining file to the next number up, and starts a new, empty `path`.
///
/// The size check happens before each write, and a write is never split across files: once the file reaches the
/// maximum size, the next write rolls it over first. A file can therefore exceed the maximum size by up to one write.
///
/// An existing file at `path` is appended to, and its size counts towards the maximum once the file is open (see
/// [`new`][Self::new] for when that is).
///
/// If rolling over fails, a warning is printed to standard error and writing continues at `path`. When `path` itself
/// couldn't be renamed, writing continues in the same file. Otherwise, the renames that succeeded stay in place and a new
/// file is started, so a rolled file whose own rename failed can be replaced by the one below it.
pub struct RollingFileWriter {
    path: PathBuf,
    max_size: u64,
    max_rolls: usize,
    file: Option<BufWriter<File>>,
    current_size: u64,
}

impl RollingFileWriter {
    /// Creates a new `RollingFileWriter` for the given path.
    ///
    /// The file, and any missing parent directories, are created on the first write. `max_size` is in bytes.
    /// `max_rolls` is the number of rolled files to keep, where `0` is treated as `1`.
    ///
    /// Because the file isn't open yet, the first write is appended to an existing file without checking its size, and
    /// the size counts from the second write on. Use [`open`][Self::open] to count it before the first write.
    pub fn new(path: impl Into<PathBuf>, max_size: u64, max_rolls: usize) -> Self {
        Self {
            path: path.into(),
            max_size,
            max_rolls: max_rolls.max(1),
            file: None,
            current_size: 0,
        }
    }

    /// Creates a new `RollingFileWriter` for the given path, and opens the file immediately.
    ///
    /// See [`new`][Self::new] for the meaning of the parameters.
    ///
    /// # Errors
    ///
    /// If the file, or any missing parent directories, can't be created, an error is returned.
    pub fn open(path: impl Into<PathBuf>, max_size: u64, max_rolls: usize) -> io::Result<Self> {
        let mut writer = Self::new(path, max_size, max_rolls);
        writer.open_if_needed()?;
        Ok(writer)
    }

    fn open_if_needed(&mut self) -> io::Result<()> {
        if self.file.is_some() {
            return Ok(());
        }

        let mut options = OpenOptions::new();
        options.append(true).create(true);
        let file = match options.open(&self.path) {
            Ok(file) => file,
            Err(e) => {
                let Some(parent) = self.path.parent() else {
                    return Err(e);
                };
                fs::create_dir_all(parent)?;
                options.open(&self.path)?
            }
        };

        self.current_size = file.metadata().map_or(0, |metadata| metadata.len());
        self.file = Some(BufWriter::new(file));
        Ok(())
    }

    fn roll_over(&mut self) -> io::Result<()> {
        // A failed flush leaves the current file open, so the buffered data isn't lost and the next write tries again.
        self.flush()?;
        self.file = None;
        self.current_size = 0;

        // The oldest rolled file doesn't exist until enough roll-overs have happened.
        let _ = fs::remove_file(rolled_path(&self.path, self.max_rolls));

        // Keep renaming after a failure, so that one stuck file doesn't stop the others from moving.
        let mut result = Ok(());
        for n in (0..self.max_rolls).rev() {
            match fs::rename(rolled_path(&self.path, n), rolled_path(&self.path, n + 1)) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => result = Err(e),
                _ => {}
            }
        }
        result?;

        self.open_if_needed()
    }
}

impl Write for RollingFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.current_size >= self.max_size {
            if let Err(e) = self.roll_over() {
                // This usually sits underneath the logging subsystem, so it can't report through it.
                eprintln!("WARNING: Failed to roll over log file '{}': {}", self.path.display(), e);
            }
        }

        self.open_if_needed()?;
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("log file should be open after opening it"));
        };
        file.write_all(buf)?;
        self.current_size = self
            .current_size
            .saturating_add(u64::try_from(buf.len()).unwrap_or(u64::MAX));
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

/// Returns the path of the `n`th rolled file, where `0` is the active file.
fn rolled_path(path: &Path, n: usize) -> PathBuf {
    if n == 0 {
        return path.to_path_buf();
    }

    let mut rolled = OsString::from(path.as_os_str());
    rolled.push(format!(".{}", n));
    PathBuf::from(rolled)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use tempfile::TempDir;

    use super::*;

    fn temp_log_path() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temporary directory should be created");
        let path = dir.path().join("test.log");
        (dir, path)
    }

    fn contents(path: &Path, n: usize) -> Option<String> {
        fs::read_to_string(rolled_path(path, n)).ok()
    }

    fn write_all(writer: &mut RollingFileWriter, writes: &[&str]) {
        for data in writes {
            writer.write_all(data.as_bytes()).expect("write should succeed");
        }
    }

    #[test]
    fn rolls_over_on_the_first_write_after_reaching_max_size() {
        let (_dir, path) = temp_log_path();
        let mut writer = RollingFileWriter::open(&path, 10, 9).expect("writer should open");

        // "0" brings the file to exactly the maximum, and the oversized write still lands whole in one file.
        write_all(&mut writer, &["12345", "6789", "0", "abcdefghijkl", "ZZZ"]);
        drop(writer);

        assert_eq!(contents(&path, 2).as_deref(), Some("1234567890"));
        assert_eq!(contents(&path, 1).as_deref(), Some("abcdefghijkl"));
        assert_eq!(contents(&path, 0).as_deref(), Some("ZZZ"));
        assert_eq!(contents(&path, 3), None);
    }

    #[test]
    fn reopening_counts_existing_contents_from_the_next_write() {
        let (_dir, path) = temp_log_path();
        let mut writer = RollingFileWriter::open(&path, 10, 9).expect("writer should open");
        write_all(&mut writer, &["12345"]);
        drop(writer);

        let mut writer = RollingFileWriter::new(&path, 10, 9);
        write_all(&mut writer, &["6789", "0", "abcdefghijkl", "ZZZ"]);
        drop(writer);

        assert_eq!(contents(&path, 2).as_deref(), Some("1234567890"));
        assert_eq!(contents(&path, 1).as_deref(), Some("abcdefghijkl"));
        assert_eq!(contents(&path, 0).as_deref(), Some("ZZZ"));
    }

    #[test]
    fn open_counts_existing_contents_before_the_first_write() {
        let (_dir, path) = temp_log_path();
        fs::write(&path, "0123456789").expect("existing log file should be written");

        let mut writer = RollingFileWriter::open(&path, 10, 1).expect("writer should open");
        write_all(&mut writer, &["new"]);
        drop(writer);

        assert_eq!(contents(&path, 1).as_deref(), Some("0123456789"));
        assert_eq!(contents(&path, 0).as_deref(), Some("new"));
    }

    #[test]
    fn new_appends_the_first_write_to_an_existing_file() {
        let (_dir, path) = temp_log_path();
        fs::write(&path, "0123456789").expect("existing log file should be written");

        let mut writer = RollingFileWriter::new(&path, 10, 1);
        write_all(&mut writer, &["a"]);
        writer.flush().expect("flush should succeed");
        assert_eq!(contents(&path, 0).as_deref(), Some("0123456789a"));
        assert_eq!(contents(&path, 1), None);

        write_all(&mut writer, &["b"]);
        drop(writer);
        assert_eq!(contents(&path, 1).as_deref(), Some("0123456789a"));
        assert_eq!(contents(&path, 0).as_deref(), Some("b"));
    }

    #[test]
    fn max_rolls_of_zero_keeps_one_rolled_file() {
        let (_dir, path) = temp_log_path();
        let mut writer = RollingFileWriter::new(&path, 1, 0);
        write_all(&mut writer, &["a", "b", "c"]);
        drop(writer);

        assert_eq!(contents(&path, 0).as_deref(), Some("c"));
        assert_eq!(contents(&path, 1).as_deref(), Some("b"));
        assert_eq!(contents(&path, 2), None);
    }

    #[test]
    fn max_size_of_zero_rolls_over_before_every_write() {
        let (_dir, path) = temp_log_path();
        let mut writer = RollingFileWriter::new(&path, 0, 2);
        write_all(&mut writer, &["a", "b", "c", "d"]);
        drop(writer);

        assert_eq!(contents(&path, 0).as_deref(), Some("d"));
        assert_eq!(contents(&path, 1).as_deref(), Some("c"));
        assert_eq!(contents(&path, 2).as_deref(), Some("b"));
        assert_eq!(contents(&path, 3), None);
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = tempfile::tempdir().expect("temporary directory should be created");

        let lazy_path = dir.path().join("lazy").join("nested").join("test.log");
        let mut writer = RollingFileWriter::new(&lazy_path, 10, 1);
        assert!(!lazy_path.exists());
        write_all(&mut writer, &["x"]);
        drop(writer);
        assert_eq!(fs::read_to_string(&lazy_path).ok().as_deref(), Some("x"));

        let eager_path = dir.path().join("eager").join("nested").join("test.log");
        let _writer = RollingFileWriter::open(&eager_path, 10, 1).expect("writer should open");
        assert!(eager_path.exists());
    }

    #[test]
    fn open_fails_when_the_parent_is_not_a_directory() {
        let dir = tempfile::tempdir().expect("temporary directory should be created");
        let blocker = dir.path().join("not-a-directory");
        fs::write(&blocker, "").expect("blocking file should be written");

        assert!(RollingFileWriter::open(blocker.join("test.log"), 10, 1).is_err());
    }

    #[test]
    fn failed_open_is_retried_on_the_next_write() {
        let dir = tempfile::tempdir().expect("temporary directory should be created");
        let blocker = dir.path().join("logs");
        fs::write(&blocker, "").expect("blocking file should be written");
        let path = blocker.join("test.log");

        let mut writer = RollingFileWriter::new(&path, 10, 1);
        assert!(writer.write_all(b"first").is_err());

        fs::remove_file(&blocker).expect("blocking file should be removed");
        write_all(&mut writer, &["second"]);
        drop(writer);
        assert_eq!(fs::read_to_string(&path).ok().as_deref(), Some("second"));
    }

    #[test]
    fn failed_roll_over_keeps_writing_to_the_current_file() {
        let (_dir, path) = temp_log_path();

        // A non-empty directory where the first rolled file belongs can be neither removed nor replaced.
        let blocker = rolled_path(&path, 1);
        fs::create_dir(&blocker).expect("blocking directory should be created");
        fs::write(blocker.join("keep"), "keep").expect("file in blocking directory should be written");

        let mut writer = RollingFileWriter::new(&path, 1, 1);
        write_all(&mut writer, &["a", "b", "c"]);
        drop(writer);

        assert_eq!(contents(&path, 0).as_deref(), Some("abc"));
        assert_eq!(fs::read_to_string(blocker.join("keep")).ok().as_deref(), Some("keep"));
    }

    #[test]
    fn partially_failed_roll_over_starts_a_new_file() {
        let (_dir, path) = temp_log_path();

        // As above, but for the second rolled file, so that only the rename into it fails.
        let blocker = rolled_path(&path, 2);
        fs::create_dir(&blocker).expect("blocking directory should be created");
        fs::write(blocker.join("keep"), "keep").expect("file in blocking directory should be written");

        let mut writer = RollingFileWriter::new(&path, 1, 2);
        write_all(&mut writer, &["a", "b", "c"]);
        drop(writer);

        // Renaming `.1` into place fails on the second roll-over, but the active file still moves to `.1`, replacing it.
        assert_eq!(contents(&path, 0).as_deref(), Some("c"));
        assert_eq!(contents(&path, 1).as_deref(), Some("b"));
        assert_eq!(fs::read_to_string(blocker.join("keep")).ok().as_deref(), Some("keep"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_flush_aborts_the_roll_over() {
        let (_dir, path) = temp_log_path();

        // Every write to `/dev/full` fails, so flushing the buffered data before rolling over fails.
        std::os::unix::fs::symlink("/dev/full", &path).expect("symlink should be created");

        let mut writer = RollingFileWriter::new(&path, 1, 1);
        write_all(&mut writer, &["a", "b"]);
        drop(writer);

        // `exists()` would follow the link, so look at the paths themselves.
        let metadata = fs::symlink_metadata(&path).expect("log path should still exist");
        assert!(metadata.file_type().is_symlink());
        assert!(fs::symlink_metadata(rolled_path(&path, 1)).is_err());
    }

    #[test]
    fn rolls_over_while_another_handle_has_the_file_open() {
        let (_dir, path) = temp_log_path();
        let mut writer = RollingFileWriter::new(&path, 1, 1);
        write_all(&mut writer, &["a"]);
        writer.flush().expect("flush should succeed");

        let _reader = File::open(&path).expect("log file should be readable");
        write_all(&mut writer, &["b"]);
        drop(writer);

        assert_eq!(contents(&path, 1).as_deref(), Some("a"));
        assert_eq!(contents(&path, 0).as_deref(), Some("b"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rolls_over_files_with_non_utf8_names() {
        use std::os::unix::ffi::OsStrExt as _;

        let dir = tempfile::tempdir().expect("temporary directory should be created");
        let path = dir.path().join(std::ffi::OsStr::from_bytes(b"test-\xff.log"));
        let mut writer = RollingFileWriter::new(&path, 1, 1);
        write_all(&mut writer, &["a", "b"]);
        drop(writer);

        let mut rolled = path.clone().into_os_string();
        rolled.push(".1");
        assert_eq!(fs::read(&rolled).ok().as_deref(), Some(&b"a"[..]));
        assert_eq!(fs::read(&path).ok().as_deref(), Some(&b"b"[..]));
    }

    /// Applies the same writes to an in-memory model of the rolling behavior, returning the expected contents of each
    /// file, where index `0` is the active file.
    fn model_files(writes: &[Vec<u8>], max_size: u64, max_rolls: usize) -> Vec<Option<Vec<u8>>> {
        let max_rolls = max_rolls.max(1);
        let mut files: Vec<Option<Vec<u8>>> = vec![None; max_rolls + 2];
        let mut current_size = 0;

        for data in writes {
            if current_size >= max_size {
                files[max_rolls] = None;
                for n in (0..max_rolls).rev() {
                    if let Some(file) = files[n].take() {
                        files[n + 1] = Some(file);
                    }
                }
                files[0] = Some(Vec::new());
            }

            let active = files[0].get_or_insert_with(Vec::new);
            active.extend_from_slice(data);
            current_size = active.len() as u64;
        }

        files
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn property_test_matches_a_model_of_size_based_rolling(
            writes in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..16), 0..48),
            max_size in 0u64..32,
            max_rolls in 0usize..4,
        ) {
            let (_dir, path) = temp_log_path();
            let mut writer = RollingFileWriter::new(&path, max_size, max_rolls);
            for data in &writes {
                prop_assert_eq!(writer.write(data).expect("write should succeed"), data.len());
            }
            drop(writer);

            let expected = model_files(&writes, max_size, max_rolls);
            for (n, expected) in expected.iter().enumerate() {
                prop_assert_eq!(&fs::read(rolled_path(&path, n)).ok(), expected, "file {}", n);
            }
        }
    }
}
