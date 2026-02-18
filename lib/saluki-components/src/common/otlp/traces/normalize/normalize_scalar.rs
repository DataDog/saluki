use super::IS_VALID_ASCII_TAG_CHAR_LOOKUP;

#[allow(missing_docs)]
pub fn is_normalized_ascii_tag_scalar(tag: &[u8]) -> bool {
    let mut i = 0;
    while i < tag.len() {
        let b = tag[i];
        if IS_VALID_ASCII_TAG_CHAR_LOOKUP[b as usize] {
            i += 1;
            continue;
        }
        if b == b'_' {
            // An underscore is only valid if it is followed by a valid non-underscore character.
            i += 1;
            if i == tag.len() || !IS_VALID_ASCII_TAG_CHAR_LOOKUP[tag[i] as usize] {
                return false;
            }
            i += 1;
            continue;
        }
        return false;
    }

    true
}
