#[allow(dead_code)] // Not in use yet
pub fn length_in_bytes(mut s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    for i in (0..max_bytes.saturating_sub(3)).rev() {
        if s.is_char_boundary(i) {
            s.truncate(i);
            if !s.ends_with("…") {
                s.push_str("…");
            }
            return s;
        }
    }
    unreachable!();
}

pub fn length_in_chars(mut s: String, max_chars: usize) -> String {
    if s.is_empty() {
        return s;
    }
    let Some(trunc_char) = max_chars.checked_sub(1) else {
        return "…".to_owned();
    };
    let mut iter = s.char_indices();
    let Some((trunc_byte, _)) = iter.nth(trunc_char) else {
        return s;
    };
    if iter.next().is_some() {
        s.truncate(trunc_byte);
        if !s.ends_with("…") {
            s.push_str("…");
        }
    }
    s
}
