//! Splitting a document into overlapping, retrievable chunks.
//!
//! Retrieval works on passages, not whole files: a chunk is small enough to be a
//! precise hit and to fit a result budget, and consecutive chunks overlap so a
//! passage split across a boundary is still wholly present in one of them.

/// Split `text` into chunks of at most `max_chars` characters, each overlapping
/// the previous by approximately `overlap` characters. Boundaries prefer
/// paragraphs, then sentences, then words; a hard character cut is used only
/// when no natural boundary exists near the target. A short text is a single
/// chunk and empty text yields none. Counting is by character, so UTF-8 is never
/// split in the middle of a code point.
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
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let hard_end = (start + max_chars).min(chars.len());
        let end = if hard_end == chars.len() {
            hard_end
        } else {
            natural_boundary(&chars, start, hard_end)
        };
        let piece: String = chars[start..end].iter().collect();
        let piece = piece.trim().to_owned();
        if !piece.is_empty() {
            chunks.push(piece);
        }
        if hard_end == chars.len() {
            break;
        }
        let mut next = end.saturating_sub(overlap).max(start + 1);
        // Do not begin a chunk halfway through a word. Advancing here can make
        // the actual overlap slightly smaller than requested, but makes every
        // retrieved passage much easier for a small generator to consume.
        while next < end && !chars[next - 1].is_whitespace() && !chars[next].is_whitespace() {
            next += 1;
        }
        while next < chars.len() && chars[next].is_whitespace() {
            next += 1;
        }
        start = next;
    }
    chunks
}

/// Choose a readable boundary in the final quarter of the available window.
fn natural_boundary(chars: &[char], start: usize, hard_end: usize) -> usize {
    let width = hard_end - start;
    let floor = start + width.saturating_mul(3) / 4;

    for index in (floor + 1..hard_end).rev() {
        if chars[index - 1] == '\n' && chars[index] == '\n' {
            return index;
        }
    }
    for index in (floor + 1..hard_end).rev() {
        if matches!(chars[index - 1], '.' | '?' | '!') && chars[index].is_whitespace() {
            return index;
        }
    }
    for index in (floor + 1..hard_end).rev() {
        if chars[index - 1] == '\n' || chars[index].is_whitespace() {
            return index;
        }
    }
    hard_end
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

    #[test]
    fn prefers_sentence_boundaries_without_losing_text() {
        let text =
            "Alpha sentence has several words. Beta sentence also has several words. Gamma ends.";
        let chunks = chunk(text, 42, 12);
        assert!(chunks[0].ends_with('.'));
        assert!(chunks.iter().all(|piece| piece.chars().count() <= 42));
        assert!(chunks.iter().any(|piece| piece.contains("Beta sentence")));
    }
}
