//! Bounds step diagnostics without splitting UTF-8 characters.

/// Most bytes in a failure message, including [`TRUNCATION_NOTICE`].
pub(crate) const MAX_MESSAGE_BYTES: usize = 4096;

pub(crate) const TRUNCATION_NOTICE: &str = "\n[message truncated]";

/// Caps `message` at [`MAX_MESSAGE_BYTES`], cutting on a character boundary and ending with [`TRUNCATION_NOTICE`].
pub(crate) fn truncate_message(mut message: String) -> String {
    if message.len() > MAX_MESSAGE_BYTES {
        let cut = message.floor_char_boundary(MAX_MESSAGE_BYTES - TRUNCATION_NOTICE.len());
        message.truncate(cut);
        message.push_str(TRUNCATION_NOTICE);
    }
    message
}
