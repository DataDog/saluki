use std::{fs::File, io::Read, path::PathBuf};

const KIB: u64 = 1024;
// AIX documents /proc data as 64-bit mode-invariant and documents future struct growth as appending fields.
// Verified against AIX 7.3 <sys/procfs.h>: offsetof(psinfo_t, pr_rssize) == 104.
const PR_RSSIZE_OFFSET: usize = 104;
const PR_RSSIZE_SIZE: usize = std::mem::size_of::<u64>();

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

        resident_set_size_from_psinfo(&buf)
    }
}

impl Default for Querier {
    fn default() -> Self {
        Self {
            psinfo_path: PathBuf::from(format!("/proc/{}/psinfo", std::process::id())),
        }
    }
}

fn resident_set_size_from_psinfo(psinfo: &[u8]) -> Option<usize> {
    let raw_rss_kib = psinfo.get(PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE)?;
    let rss_kib = u64::from_ne_bytes(raw_rss_kib.try_into().ok()?);
    let rss_bytes = rss_kib.checked_mul(KIB)?;

    usize::try_from(rss_bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::{resident_set_size_from_psinfo, Querier, KIB, PR_RSSIZE_OFFSET, PR_RSSIZE_SIZE};

    #[test]
    fn basic() {
        let mut querier = Querier::default();
        assert!(querier.resident_set_size().is_some());
    }

    #[test]
    fn parses_resident_set_size_from_psinfo() {
        let rss_kib = 7u64;
        let mut psinfo = [0; PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE];
        psinfo[PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&rss_kib.to_ne_bytes());

        assert_eq!(resident_set_size_from_psinfo(&psinfo), Some((rss_kib * KIB) as usize));
    }

    #[test]
    fn parses_resident_set_size_from_documented_offset() {
        const PR_SIZE_OFFSET: usize = 96;
        const PR_START_OFFSET: usize = 112;

        let rss_kib = 7u64;
        let mut psinfo = [0; PR_START_OFFSET + PR_RSSIZE_SIZE];
        psinfo[PR_SIZE_OFFSET..PR_SIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&3u64.to_ne_bytes());
        psinfo[PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&rss_kib.to_ne_bytes());
        psinfo[PR_START_OFFSET..PR_START_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&99u64.to_ne_bytes());

        assert_eq!(resident_set_size_from_psinfo(&psinfo), Some((rss_kib * KIB) as usize));
    }

    #[test]
    fn converts_resident_set_size_from_kib_not_pages() {
        let rss_kib = 7u64;
        let mut psinfo = [0; PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE];
        psinfo[PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&rss_kib.to_ne_bytes());

        assert_eq!(resident_set_size_from_psinfo(&psinfo), Some(7168));
    }

    #[test]
    #[cfg(target_endian = "big")]
    fn parses_big_endian_resident_set_size() {
        let mut psinfo = [0; PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE];
        psinfo[PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0, 7]);

        assert_eq!(resident_set_size_from_psinfo(&psinfo), Some(7168));
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn parses_little_endian_resident_set_size() {
        let mut psinfo = [0; PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE];
        psinfo[PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&[7, 0, 0, 0, 0, 0, 0, 0]);

        assert_eq!(resident_set_size_from_psinfo(&psinfo), Some(7168));
    }

    #[test]
    fn accepts_zero_resident_set_size() {
        let mut psinfo = [0; PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE];
        psinfo[PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&0u64.to_ne_bytes());

        assert_eq!(resident_set_size_from_psinfo(&psinfo), Some(0));
    }

    #[test]
    fn rejects_truncated_psinfo() {
        assert_eq!(resident_set_size_from_psinfo(&[]), None);
    }

    #[test]
    fn rejects_overflowing_resident_set_size() {
        let mut psinfo = [0; PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE];
        psinfo[PR_RSSIZE_OFFSET..PR_RSSIZE_OFFSET + PR_RSSIZE_SIZE].copy_from_slice(&u64::MAX.to_ne_bytes());

        assert_eq!(resident_set_size_from_psinfo(&psinfo), None);
    }
}
