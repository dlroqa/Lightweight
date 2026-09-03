//! Splitting a document into overlapping, retrievable chunks.
//!
//! Retrieval works on passages, not whole files: a chunk is small enough to be a
//! precise hit and to fit a result budget, and consecutive chunks overlap so a
//! passage split across a boundary is still wholly present in one of them.

/// Split `text` into chunks of at most `max_chars` characters, each overlapping
/// the previous by `overlap` characters. A short text is a single chunk; empty
/// text yields none. Counting is by character, so a boundary never splits one.
pub fn chunk(text: &str, max_chars: usize, overlap: usize) -> Vec<String> {
    let text = text.trim();
    let max_chars = max_chars.max(1);
    let overlap = overlap.min(max_chars - 1);
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    if chars.len() <= max_chars {
        return vec![text.to_owned()];
    }
    let step = (max_chars - overlap).max(1);
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let end = (start + max_chars).min(chars.len());
        let piece: String = chars[start..end].iter().collect();
        let piece = piece.trim().to_owned();
        if !piece.is_empty() {
            chunks.push(piece);
        }
        if end == chars.len() {
            break;
        }
        start += step;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_text_is_one_chunk() {
        assert_eq!(chunk("hello world", 100, 20), vec!["hello world"]);
        assert!(chunk("   ", 100, 20).is_empty());
    }

    #[test]
    fn long_text_splits_with_overlap() {
        let text: String = (0..100).map(|n| format!("word{n} ")).collect();
        let chunks = chunk(&text, 60, 15);
        assert!(chunks.len() > 1, "long text should split");
        for piece in &chunks {
            assert!(piece.chars().count() <= 60);
        }
    }
}
