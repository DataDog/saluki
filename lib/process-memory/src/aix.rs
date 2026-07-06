use std::{fs::File, io::Read, path::PathBuf};

// Offset of `pr_rssize` in the 64-bit AIX 7.3 `psinfo_t` structure.
const PR_RSSIZE_OFFSET: usize = 104;
const PR_RSSIZE_SIZE: usize = std::mem::size_of::<i64>();

/// A memory usage querier.
pub struct Querier {
    psinfo_path: PathBuf,
}

impl Querier {
    /// Gets the resident set size of this process, in bytes.
    ///
    /// If the resident set size can't be determined, `None` is returned. This could be for a number of underlying
    /// reasons, but should generally be considered an incredibly rare/unlikely event.
    pub fn resident_set_size(&mut self) -> Option<usize> {
        let mut file = File::open(&self.psinfo_path).ok()?;
        let mut buf = [0; PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE];
        file.read_exact(&mut buf).ok()?;

        resident_set_size_from_psinfo(&buf, page_size()?)
    }
}

impl Default for Querier {
    fn default() -> Self {
        Self {
            psinfo_path: PathBuf::from(format!("/proc/{}/psinfo", std::process::id())),
        }
    }
}

fn page_size() -> Option<usize> {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        None
    } else {
        Some(page_size as usize)
    }
}

fn resident_set_size_from_psinfo(psinfo: &[u8], page_size: usize) -> Option<usize> {
    let raw_rss_pages = psinfo.get(PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE)?;
    let rss_pages = i64::from_ne_bytes(raw_rss_pages.try_into().ok()?);
    let rss_pages = usize::try_from(rss_pages).ok()?;

    rss_pages.checked_mul(page_size)
}

#[cfg(test)]
mod tests {
    use super::{resident_set_size_from_psinfo, Querier, PR_RSSIZE_OFFSET, PR_RSSIZE_SIZE};

    #[test]
    fn basic() {
        let mut querier = Querier::default();
        assert!(querier.resident_set_size().is_some());
    }

    #[test]
    fn parses_resident_set_size_from_psinfo() {
        let page_size = 4096;
        let rss_pages = 7i64;
        let mut psinfo = [0; PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE];
        psinfo[PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&rss_pages.to_ne_bytes());

        assert_eq!(resident_set_size_from_psinfo(&psinfo, page_size), Some(28672));
    }

    #[test]
    fn rejects_truncated_psinfo() {
        assert_eq!(resident_set_size_from_psinfo(&[], 4096), None);
    }

    #[test]
    fn rejects_negative_resident_set_size() {
        let mut psinfo = [0; PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE];
        psinfo[PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&(-1i64).to_ne_bytes());

        assert_eq!(resident_set_size_from_psinfo(&psinfo, 4096), None);
    }
}
