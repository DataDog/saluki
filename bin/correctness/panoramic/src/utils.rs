/// Removes ANSI escape sequences (`ESC[...letter`) from a byte slice.
pub(crate) fn strip_ansi_codes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b && input.get(i + 1) == Some(&b'[') {
            i += 2;
            while i < input.len() && !input[i].is_ascii_alphabetic() {
                i += 1;
            }
            i += 1;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
}
