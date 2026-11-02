//! Splitting a document into chunks, and the content hash that lets the worker
//! re-embed only what changed (ADR 0019, brick 3). Pure and dependency-free.

/// Split `text` into chunks of about `size` characters with `overlap` characters
/// shared between neighbours. Counts **characters**, not bytes, so a multi-byte
/// character is never split; always advances (the derive and schema checks keep
/// `overlap < size`, but this clamps anyway so it cannot loop).
pub fn chunk(text: &str, size: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let size = size.max(1);
    let step = size.saturating_sub(overlap).max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + size).min(chars.len());
        out.push(chars[start..end].iter().collect());
        if end == chars.len() {
            break;
        }
        start += step;
    }
    out
}

/// A stable 64-bit FNV-1a hash of a chunk, as hex. Deterministic across runs and
/// processes (unlike `DefaultHasher`, whose guarantees are not portable), which is
/// what lets one worker trust a hash another worker wrote and skip re-embedding
/// an unchanged chunk.
pub fn content_hash(s: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_text_is_one_chunk() {
        assert_eq!(chunk("hello", 800, 100), vec!["hello".to_string()]);
    }

    #[test]
    fn chunks_overlap_and_cover_the_whole_text() {
        let text: String = ('a'..='z').collect(); // 26 chars
        let cs = chunk(&text, 10, 3); // step = 7
        assert_eq!(cs[0], "abcdefghij");
        assert_eq!(cs[1], "hijklmnopq"); // starts at 7, overlaps "hij"
                                         // Every character of the source appears in some chunk.
        let joined: String = cs.join("");
        for c in text.chars() {
            assert!(joined.contains(c));
        }
    }

    #[test]
    fn an_empty_text_yields_no_chunks() {
        assert!(chunk("", 800, 100).is_empty());
    }

    #[test]
    fn a_multibyte_boundary_is_never_split() {
        // Five 3-byte chars; a byte-based split would panic or corrupt.
        let text = "日本語です。";
        let cs = chunk(text, 2, 0);
        assert_eq!(cs, vec!["日本", "語で", "す。"]);
    }

    #[test]
    fn the_hash_is_stable_and_changes_with_content() {
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
        // 16 hex chars, and pinned so a refactor cannot silently change the
        // algorithm (which would invalidate every stored hash at once). The FNV-1a
        // 64-bit of "abc" is a well-known constant.
        assert_eq!(content_hash("abc").len(), 16);
        assert_eq!(content_hash("abc"), "e71fa2190541574b");
    }
}
