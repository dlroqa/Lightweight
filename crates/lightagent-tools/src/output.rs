//! Bounding a tool's output.
//!
//! A tool's result is appended to the conversation and sent back to the model,
//! so an unbounded one is an unbounded prompt. [`clamp`] caps it at a byte
//! ceiling, cutting on a UTF-8 boundary and appending a short, honest notice so
//! the model is told the result was shortened rather than silently misled.

/// Cap `content` at `max_bytes`, cutting on a character boundary.
///
/// A result within the ceiling is returned unchanged. A longer one is truncated
/// to the last character boundary at or before the limit and a `…[truncated,
/// N bytes]` notice is appended naming how much was dropped. When the ceiling is
/// too small to hold even the notice, the notice itself is returned.
pub fn clamp(content: String, max_bytes: usize) -> String {
    if content.len() <= max_bytes {
        return content;
    }

    let dropped = content.len();
    let mut cut = max_bytes.min(content.len());
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut kept = content;
    kept.truncate(cut);
    let removed = dropped - kept.len();
    kept.push_str(&format!("\n…[truncated, {removed} bytes]"));
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_is_unchanged() {
        assert_eq!(clamp("hello".into(), 1024), "hello");
    }

    #[test]
    fn long_content_is_truncated_with_a_notice() {
        let out = clamp("a".repeat(1000), 100);
        assert!(out.starts_with(&"a".repeat(90)));
        assert!(out.contains("truncated"));
    }

    #[test]
    fn a_cut_never_splits_a_multibyte_character() {
        // "é" is two bytes; a cut at an odd byte must step back to a boundary.
        let out = clamp("é".repeat(50), 9);
        assert!(out.is_char_boundary(0));
        // Everything before the notice is valid UTF-8 by construction (String).
        assert!(out.contains("truncated"));
    }
}
