pub fn attr(s: &str) -> String {
    let mut result = Vec::with_capacity(
        s.bytes()
            .map(|b| match b {
                b'"' => 6,
                b'&' => 5,
                _ => 1,
            })
            .sum(),
    );
    for b in s.bytes() {
        match b {
            b'"' => result.extend(b"&quot;"),
            b'&' => result.extend(b"&amp;"),
            _ => result.push(b),
        }
    }
    String::from_utf8(result).unwrap()
}

pub fn text(s: &str) -> String {
    let mut result = Vec::with_capacity(
        s.bytes()
            .map(|b| match b {
                b'&' => 5,
                b'<' | b'>' => 4,
                _ => 1,
            })
            .sum(),
    );
    for b in s.bytes() {
        match b {
            b'&' => result.extend(b"&amp;"),
            b'<' => result.extend(b"&lt;"),
            b'>' => result.extend(b"&gt;"),
            _ => result.push(b),
        }
    }
    String::from_utf8(result).unwrap()
}
