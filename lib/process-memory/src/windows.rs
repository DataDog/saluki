use std::mem::MaybeUninit;

use windows_sys::Win32::System::{
    ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
    Threading::GetCurrentProcess,
};

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
        // Prepare a holding struct for the process memory counters.
        let mut pmc = MaybeUninit::<PROCESS_MEMORY_COUNTERS>::uninit();
        let pmc_len = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;

        let result = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), pmc.as_mut_ptr(), pmc_len) };
        match result {
            // Failed to get the process memory counters.
            //
            // This could be for a number of reasons, but should generally be considered an incredibly rare/unlikely event.
            0 => None,

            _ => {
                // SAFETY: We know the structure has been populated by `process_memory_counters` at this point.
                let pmc = unsafe { pmc.assume_init() };
                Some(pmc.WorkingSetSize)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Querier;

    #[test]
    fn basic() {
        let mut querier = Querier::default();
        assert!(querier.resident_set_size().is_some());
    }
}
