//! Turning text into a vector for cosine retrieval, with no model and no
//! dependency.
//!
//! [`HashingEmbedder`] is a *feature-hashing bag-of-words*: each token is hashed
//! into one of [`DIM`] buckets (a sign bit halves collision bias), the buckets
//! are summed and the vector is L2-normalized, so a dot product is a cosine
//! similarity. This is lexical, not semantic — it matches shared words, not
//! shared meaning — but it is deterministic, offline, and identical for a query
//! and a document, which is what retrieval needs. The [`Embedder`] trait leaves
//! room for a semantic backend later without changing the store or the tool.

/// The fixed vector dimension. Stored vectors of another dimension are ignored.
pub const DIM: usize = 1024;

/// Anything that maps text to a fixed-length, comparable vector.
pub trait Embedder: Send + Sync {
    /// Embed `text`; the result has length [`DIM`] and unit norm (or all zeros
    /// when the text has no tokens).
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// The dependency-free feature-hashing embedder.
#[derive(Clone, Copy, Debug, Default)]
pub struct HashingEmbedder;

impl Embedder for HashingEmbedder {
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0f32; DIM];
        for token in tokenize(text) {
            let hash = fnv1a(token.as_bytes());
            let index = (hash % DIM as u64) as usize;
            let sign = if (hash >> 63) & 1 == 1 { -1.0 } else { 1.0 };
            vector[index] += sign;
        }
        l2_normalize(&mut vector);
        vector
    }
}

/// Cosine similarity of two unit vectors (their dot product). Zero when either
/// is empty or a different length.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Lowercase alphanumeric tokens of length ≥ 2.
fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() >= 2)
        .map(|word| word.to_lowercase())
}

fn l2_normalize(vector: &mut [f32]) {
    let norm: f32 = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_is_maximally_similar() {
        let e = HashingEmbedder;
        let a = e.embed("the quick brown fox jumps");
        assert!((cosine(&a, &a) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn related_beats_unrelated() {
        let e = HashingEmbedder;
        let query = e.embed("rust async runtime tokio");
        let related = e.embed("tokio is an async runtime for rust");
        let unrelated = e.embed("bananas are a yellow tropical fruit");
        assert!(
            cosine(&query, &related) > cosine(&query, &unrelated),
            "the related passage must score higher"
        );
    }

    #[test]
    fn empty_text_is_a_zero_vector() {
        let v = HashingEmbedder.embed("  !! ");
        assert_eq!(v.len(), DIM);
        assert!(v.iter().all(|x| *x == 0.0));
    }
}
