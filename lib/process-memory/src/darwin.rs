use std::mem::MaybeUninit;

use libc::{
    mach_msg_type_number_t, mach_task_basic_info_data_t, mach_task_self, task_info, task_info_t,
    KERN_SUCCESS, MACH_TASK_BASIC_INFO, MACH_TASK_BASIC_INFO_COUNT,
};

/// A memory usage querier.
#[derive(Default)]
pub struct Querier;

impl Querier {
    /// Gets the resident set size of this process, in bytes.
    ///
    /// If the resident set size cannot be determined, `None` is returned. This could be for a number of underlying
    /// reasons, but should generally be considered an incredibly rare/unlikely event.
    pub fn resident_set_size(&mut self) -> Option<usize> {
        // Prepare a holding struct for the task info.
        //
        // This represents a set of integers, each relating to a specific task value, and `task_info` expects a pointer to
        // this struct and the number of integers it is able to write into it, which is already derived for us in
        // `MACH_TASK_BASIC_INFO_COUNT`.
        let mut basic_task_info = MaybeUninit::<mach_task_basic_info_data_t>::uninit();
        let mut basic_task_info_len = MACH_TASK_BASIC_INFO_COUNT;

        // SAFETY: We're passing a valid pointer, and struct length, for the task info output.
        let result = unsafe {
            task_info(
                mach_task_self(),
                MACH_TASK_BASIC_INFO,
                basic_task_info.as_mut_ptr() as task_info_t,
                &mut basic_task_info_len as *mut mach_msg_type_number_t,
            )
        };
        match result {
            KERN_SUCCESS => {
                // SAFETY: We know the structure has been populated by `task_info` at this point.
                let basic_task_info = unsafe { basic_task_info.assume_init() };
                Some(basic_task_info.resident_size as usize)
            }

            // Failed to get the task info.
            //
            // This could be for a number of reasons, but should generally be considered an incredibly rare/unlikely event.
            _ => None,
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
