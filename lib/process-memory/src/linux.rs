use std::{
    fs::{self, File},
    io::{self, Read},
    mem::MaybeUninit,
};

const SMAPS_ROLLUP_PATH: &str = "/proc/self/smaps_rollup";
const SMAPS_PATH: &str = "/proc/self/smaps";
const STATM: &str = "/proc/self/statm";
const RSS_LINE_PREFIX: &[u8] = b"Rss: ";

enum StatSource {
    SmapsRollup(Scanner<File>),
    Smaps(Scanner<File>),
    Statm(usize),
}

/// A memory usage querier.
pub struct Querier {
    source: StatSource,
}

impl Querier {
    /// Gets the resident set size of this process, in bytes.
    ///
    /// If the resident set size cannot be determined, `None` is returned. This could be for a number of underlying
    /// reasons, but should generally be considered an incredibly rare/unlikely event.
    pub fn resident_set_size(&mut self) -> Option<usize> {
        match &mut self.source {
            StatSource::SmapsRollup(scanner) => {
                // As smaps_rollup is a pre-aggregated version of smaps, there's only one "Rss:" line that we need to find, so use
                // the same scanner-based approach as we do for smaps, but just take the first matching line we find.
                scanner.reset_with_path(SMAPS_ROLLUP_PATH).ok()?;

                while let Ok(Some(raw_rss_line)) = scanner.next_matching_line(RSS_LINE_PREFIX) {
                    let raw_rss_value = skip_to_line_value(raw_rss_line)?;
                    if let Some(rss_bytes) = parse_kb_value_as_bytes(raw_rss_value) {
                        return Some(rss_bytes);
                    }
                }

                None
            }
            StatSource::Smaps(scanner) => {
                // Scan all lines in smaps, looking for lines that start with "Rss:". Each of these lines will contain the resident
                // set size of a particular memory mapping. We simply need to find all of these lines and aggregate their value to
                // get the RSS for the process.
                scanner.reset_with_path(SMAPS_PATH).ok()?;

                let mut total_rss_bytes = 0;
                while let Ok(Some(raw_rss_line)) = scanner.next_matching_line(RSS_LINE_PREFIX) {
                    let raw_rss_value = skip_to_line_value(raw_rss_line)?;
                    if let Some(rss_bytes) = parse_kb_value_as_bytes(raw_rss_value) {
                        total_rss_bytes += rss_bytes;
                    }
                }

                if total_rss_bytes > 0 {
                    Some(total_rss_bytes)
                } else {
                    None
                }
            }
            StatSource::Statm(page_size) => {
                // Unlike smaps/smaps_rollup, statm is a drastically simpler format that is written as a single line with
                // space-delimited fields. Since we have no lines to scan, it's much simpler to just read the entire file into a
                // stack-allocated buffer.
                //
                // With seven integer fields, we can napkin math this to wanting to hold 20 bytes per field, plus the separators and
                // newline, which is 153... but power-of-two numbers somehow feel better, so we'll go to 256.
                let mut buf = [0; 256];
                let mut file = File::open(STATM).ok()?;
                let n = file.read(&mut buf).ok()?;
                if n == 0 || n == buf.len() {
                    // If we read no bytes, or filled the entire buffer, something is very wrong.
                    return None;
                }

                // Resident set size is the second field, so we need to skip to it.
                let raw_rss_field = buf.split(|b| *b == b' ').nth(1)?;

                // We need to parse the field as an integer, and then multiply it by the page size to get the value in bytes.
                let rss_pages = std::str::from_utf8(raw_rss_field).ok()?.parse::<usize>().ok()?;
                Some(rss_pages * *page_size)
            }
        }
    }
}

impl Default for Querier {
    fn default() -> Self {
        Self {
            source: determine_stat_source(),
        }
    }
}

fn determine_stat_source() -> StatSource {
    if fs::metadata(SMAPS_ROLLUP_PATH).is_ok() {
        StatSource::SmapsRollup(Scanner::new())
    } else if fs::metadata(SMAPS_PATH).is_ok() {
        StatSource::Smaps(Scanner::new())
    } else {
        StatSource::Statm(page_size())
    }
}

fn page_size() -> usize {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        0
    } else {
        page_size as usize
    }
}

fn parse_kb_value_as_bytes(raw_rss_value: &[u8]) -> Option<usize> {
    // The raw value here will be in the form of `XXXXXX kB`, so we want to find the first whitespace character, and
    // take everything before that.
    match raw_rss_value.iter().position(|&b| b == b' ') {
        Some(space_idx) => {
            let raw_value = &raw_rss_value[..space_idx];
            std::str::from_utf8(raw_value)
                .ok()?
                .parse::<usize>()
                .ok()
                .map(|value| value * 1024)
        }
        None => None,
    }
}

struct Scanner<T> {
    io: Option<T>,
    eof: bool,
    buf: Vec<u8>,
    pending_consume: Option<usize>,
}

impl<T> Scanner<T>
where
    T: Read,
{
    fn new() -> Self {
        Self {
            io: None,
            eof: false,
            buf: Vec::with_capacity(8192),
            pending_consume: None,
        }
    }

    fn reset(&mut self, io: T) {
        self.buf.clear();
        self.eof = false;
        self.pending_consume = None;
        self.io = Some(io);
    }

    fn get_io_mut(&mut self) -> io::Result<&mut T> {
        match self.io.as_mut() {
            Some(io) => Ok(io),
            None => Err(io::Error::new(io::ErrorKind::Other, "no file set in scanner")),
        }
    }

    fn fill_buf(&mut self) -> io::Result<()> {
        if self.eof {
            return Ok(());
        }

        // If our buffer isn't entirely filled, try and fill the remainder.
        if self.buf.len() < self.buf.capacity() {
            // If we have any spare capacity, try to read as many bytes as we can hold in it.
            //
            // SAFETY: There's no invalid bit patterns for `u8`.
            let read_buf = unsafe { &mut *(self.buf.spare_capacity_mut() as *mut [MaybeUninit<u8>] as *mut [u8]) };
            let n = self.get_io_mut()?.read(read_buf)?;
            if n == 0 {
                self.eof = true;
            }

            // SAFETY: We've just read `n` bytes into `buf`, based on the spare capacity, so incrementing our length by
            // `n` will only cover initialized bytes, and can't result in a length greater than the buffer capacity.
            unsafe {
                self.buf.set_len(self.buf.len() + n);
            }
        }

        Ok(())
    }

    fn next_matching_line(&mut self, prefix: &[u8]) -> io::Result<Option<&[u8]>> {
        loop {
            // We've reached EOF and have processed the entire file.
            if self.eof && self.buf.is_empty() {
                return Ok(None);
            }

            // If we have a pending consume, take that many bytes from the front of the buffer and shift the rest of the
            // data forward.
            if let Some(consume) = self.pending_consume {
                self.buf.drain(..consume);
                self.pending_consume = None;
            }

            // Ensure our buffer is as filled as it can be.
            self.fill_buf()?;

            // Get the entire buffer that we currently have, and see if it starts with our prefix.
            //
            // If it does, we then need to also find a newline character to know where to chop it off.
            if self.buf.starts_with(prefix) {
                let maybe_newline_idx = self.buf.iter().position(|&b| b == b'\n');
                if let Some(newline_idx) = maybe_newline_idx {
                    // Consume up to and including the newline character, but only hand back the bytes up to the newline.
                    self.pending_consume = Some(newline_idx + 1);

                    return Ok(Some(&self.buf[..newline_idx]));
                }
            } else {
                // Our buffer doesn't start with the prefix, so we need to find the next newline character and consume
                // up to that point to reset ourselves.
                //
                // We're essentially resetting ourselves to start at the next line in the file.
                let maybe_newline_idx = self.buf.iter().position(|&b| b == b'\n');
                if let Some(newline_idx) = maybe_newline_idx {
                    self.pending_consume = Some(newline_idx + 1);
                } else {
                    // We couldn't find a newline character. If we're at EOF, then there's no more data for us to
                    // potentially read in that might contain a newline character... so we're done. Just clear the
                    // buffer and return `None`.
                    //
                    // Otherwise, we continue looping in order to read more data and skip through until we find the next
                    // line.
                    if self.eof {
                        self.buf.clear();
                        return Ok(None);
                    }
                }
            }
        }
    }
}

impl Scanner<File> {
    fn reset_with_path(&mut self, path: &str) -> io::Result<()> {
        let file = File::open(path)?;
        self.reset(file);

        Ok(())
    }
}

fn skip_to_line_value(raw_line: &[u8]) -> Option<&[u8]> {
    // We skip over all non-numeric characters and then return what's left.
    //
    // If the line doesn't contain any numeric characters, `None` is returned.
    raw_line
        .iter()
        .position(|b| b.is_ascii_digit())
        .map(|idx| &raw_line[idx..])
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::Querier;

    #[test]
    fn basic() {
        let mut querier = Querier::default();
        assert!(querier.resident_set_size().is_some());
    }
}
