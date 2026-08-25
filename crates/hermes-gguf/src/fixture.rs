//! A GGUF *writer*, for building test fixtures.
//!
//! Two reasons this exists rather than a directory of committed sample files:
//!
//! 1. Fixtures are generated, so tests need no network and no multi-gigabyte
//!    downloads. A complete model with a tokenizer vocabulary and a chat
//!    template comes to a few hundred kilobytes.
//! 2. It is a **second, independent implementation of the format**. The reader
//!    was written from the layout in `gguf.cpp`; this writer was too, and every
//!    round-trip test asserts they agree. A misreading that affected only one
//!    of them shows up immediately.
//!
//! Only compiled for tests, or with the `fixtures` feature.

use std::io::Write;
use std::path::Path;

use hermes_core::GgmlType;

use crate::reader::{DEFAULT_ALIGNMENT, MAGIC, MAX_VERSION};

/// Ceiling on a fixture's tensor data. Fixtures exist to be small and fast;
/// anything approaching this is a mistake in the test, not a real model.
const MAX_FIXTURE_DATA_BYTES: u64 = 64 * 1024 * 1024;

/// A value to write into fixture metadata.
#[derive(Clone, Debug)]
pub enum FixtureValue {
    U8(u8),
    I8(i8),
    U16(u16),
    I16(i16),
    U32(u32),
    I32(i32),
    U64(u64),
    I64(i64),
    F32(f32),
    F64(f64),
    Bool(bool),
    Str(String),
    U32Array(Vec<u32>),
    U64Array(Vec<u64>),
    F32Array(Vec<f32>),
    StrArray(Vec<String>),
}

impl From<&str> for FixtureValue {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

impl From<u32> for FixtureValue {
    fn from(value: u32) -> Self {
        Self::U32(value)
    }
}

impl From<u64> for FixtureValue {
    fn from(value: u64) -> Self {
        Self::U64(value)
    }
}

impl From<f32> for FixtureValue {
    fn from(value: f32) -> Self {
        Self::F32(value)
    }
}

impl From<bool> for FixtureValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

/// A tensor to declare in the header. Its data is written as zeros.
#[derive(Clone, Debug)]
pub struct FixtureTensor {
    pub name: String,
    pub dims: Vec<u64>,
    pub ggml_type: GgmlType,
}

/// Builds a syntactically valid GGUF file.
#[derive(Clone, Debug)]
pub struct GgufBuilder {
    version: u32,
    alignment: u64,
    metadata: Vec<(String, FixtureValue)>,
    tensors: Vec<FixtureTensor>,
}

impl Default for GgufBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl GgufBuilder {
    pub fn new() -> Self {
        Self {
            version: MAX_VERSION,
            alignment: DEFAULT_ALIGNMENT,
            metadata: Vec::new(),
            tensors: Vec::new(),
        }
    }

    /// Write a deliberately wrong version, to exercise the version check.
    pub fn version(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// Set `general.alignment`. Also used to write an invalid one on purpose.
    pub fn alignment(mut self, alignment: u64) -> Self {
        self.alignment = alignment;
        self.kv("general.alignment", FixtureValue::U32(alignment as u32))
    }

    pub fn kv(mut self, key: &str, value: impl Into<FixtureValue>) -> Self {
        self.metadata.push((key.to_owned(), value.into()));
        self
    }

    pub fn tensor(mut self, name: &str, dims: &[u64], ggml_type: GgmlType) -> Self {
        self.tensors.push(FixtureTensor {
            name: name.to_owned(),
            dims: dims.to_vec(),
            ggml_type,
        });
        self
    }

    /// A small but structurally complete model.
    ///
    /// Every key an architecture-driven reader looks for is present, spelled
    /// with the `{arch}.` prefix, so tests exercise the interpolation rather
    /// than a hard-coded key list.
    pub fn small_model(architecture: &str) -> Self {
        let vocab: Vec<String> = (0..128).map(|index| format!("tok{index}")).collect();
        Self::new()
            .kv("general.architecture", architecture)
            .kv("general.name", "fixture-model")
            .kv(&format!("{architecture}.context_length"), 4096u32)
            .kv(&format!("{architecture}.block_count"), 2u32)
            .kv(&format!("{architecture}.embedding_length"), 64u32)
            .kv(&format!("{architecture}.feed_forward_length"), 256u32)
            .kv(&format!("{architecture}.attention.head_count"), 8u32)
            .kv(&format!("{architecture}.attention.head_count_kv"), 2u32)
            .kv(&format!("{architecture}.rope.freq_base"), 10000.0f32)
            .kv("tokenizer.ggml.model", "llama")
            .kv("tokenizer.ggml.bos_token_id", 1u32)
            .kv("tokenizer.ggml.eos_token_id", 2u32)
            .kv("tokenizer.ggml.tokens", FixtureValue::StrArray(vocab))
            .kv(
                "tokenizer.chat_template",
                "{% for m in messages %}{{ m }}{% endfor %}",
            )
            .tensor("token_embd.weight", &[64, 128], GgmlType::Q4_K)
            .tensor("blk.0.attn_q.weight", &[64, 64], GgmlType::Q4_K)
            .tensor("blk.1.attn_q.weight", &[64, 64], GgmlType::Q4_K)
            .tensor("output_norm.weight", &[64], GgmlType::F32)
    }

    /// Encode to bytes.
    pub fn build(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&self.version.to_le_bytes());
        out.extend_from_slice(&(self.tensors.len() as u64).to_le_bytes());
        out.extend_from_slice(&(self.metadata.len() as u64).to_le_bytes());

        for (key, value) in &self.metadata {
            write_string(&mut out, key);
            write_value(&mut out, value);
        }

        let mut data_size = 0u64;
        for tensor in &self.tensors {
            write_string(&mut out, &tensor.name);
            out.extend_from_slice(&(tensor.dims.len() as u32).to_le_bytes());
            for dim in &tensor.dims {
                out.extend_from_slice(&dim.to_le_bytes());
            }
            out.extend_from_slice(&tensor.ggml_type.id().to_le_bytes());
            out.extend_from_slice(&data_size.to_le_bytes());

            let elements: u64 = tensor.dims.iter().product();
            let bytes = tensor
                .ggml_type
                .bytes_for_elements(elements)
                .unwrap_or(elements);
            data_size = data_size.saturating_add(bytes);
        }

        if !self.tensors.is_empty() && self.alignment > 0 {
            let remainder = out.len() as u64 % self.alignment;
            if remainder != 0 {
                out.resize(out.len() + (self.alignment - remainder) as usize, 0);
            }
        }

        // Zeroed tensor data, so the file's length matches its header and the
        // reader's data-section arithmetic can be checked against reality.
        //
        // That materialization is why fixtures must stay small: declaring a
        // billion-element tensor here would try to allocate gigabytes. A
        // fixture that large is always an authoring mistake, and saying so is
        // far more useful than being killed by the OOM killer mid-suite.
        assert!(
            data_size <= MAX_FIXTURE_DATA_BYTES,
            "fixture declares {data_size} bytes of tensor data, over the \
             {MAX_FIXTURE_DATA_BYTES} byte limit; use smaller tensor extents"
        );
        out.resize(out.len() + data_size as usize, 0);
        out
    }

    /// Encode and write to disk.
    pub fn write_to(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        let mut file = std::fs::File::create(path)?;
        file.write_all(&self.build())?;
        file.flush()
    }
}

fn write_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(&(value.len() as u64).to_le_bytes());
    out.extend_from_slice(value.as_bytes());
}

fn write_value(out: &mut Vec<u8>, value: &FixtureValue) {
    // Tags match `enum gguf_type` in gguf.h.
    match value {
        FixtureValue::U8(v) => {
            out.extend_from_slice(&0u32.to_le_bytes());
            out.push(*v);
        }
        FixtureValue::I8(v) => {
            out.extend_from_slice(&1u32.to_le_bytes());
            out.push(*v as u8);
        }
        FixtureValue::U16(v) => {
            out.extend_from_slice(&2u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        FixtureValue::I16(v) => {
            out.extend_from_slice(&3u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        FixtureValue::U32(v) => {
            out.extend_from_slice(&4u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        FixtureValue::I32(v) => {
            out.extend_from_slice(&5u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        FixtureValue::F32(v) => {
            out.extend_from_slice(&6u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        FixtureValue::Bool(v) => {
            out.extend_from_slice(&7u32.to_le_bytes());
            out.push(u8::from(*v));
        }
        FixtureValue::Str(v) => {
            out.extend_from_slice(&8u32.to_le_bytes());
            write_string(out, v);
        }
        FixtureValue::U64(v) => {
            out.extend_from_slice(&10u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        FixtureValue::I64(v) => {
            out.extend_from_slice(&11u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        FixtureValue::F64(v) => {
            out.extend_from_slice(&12u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        FixtureValue::U32Array(items) => {
            write_array_header(out, 4, items.len());
            for item in items {
                out.extend_from_slice(&item.to_le_bytes());
            }
        }
        FixtureValue::U64Array(items) => {
            write_array_header(out, 10, items.len());
            for item in items {
                out.extend_from_slice(&item.to_le_bytes());
            }
        }
        FixtureValue::F32Array(items) => {
            write_array_header(out, 6, items.len());
            for item in items {
                out.extend_from_slice(&item.to_le_bytes());
            }
        }
        FixtureValue::StrArray(items) => {
            write_array_header(out, 8, items.len());
            for item in items {
                write_string(out, item);
            }
        }
    }
}

fn write_array_header(out: &mut Vec<u8>, element_tag: u32, len: usize) {
    out.extend_from_slice(&9u32.to_le_bytes());
    out.extend_from_slice(&element_tag.to_le_bytes());
    out.extend_from_slice(&(len as u64).to_le_bytes());
}

/// A temporary directory that cleans up after itself.
pub struct TempDir(std::path::PathBuf);

/// The name of a fixture directory, from a clock reading it is *given*.
///
/// Taking the instant as an argument rather than reading it is what makes the
/// invariant testable: hand it the same instant twice and the two names must
/// still differ. That is not a hypothetical - it is precisely what a platform
/// whose timer is coarser than this call does, and the sequence number is the
/// part that answers it. The pid separates the several test binaries cargo runs
/// at once; the instant is kept only so a leftover directory says when it is
/// from.
fn directory_name(tag: &str, nanos: u128) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "hermes-gguf-{tag}-{}-{nanos}-{sequence}",
        std::process::id()
    )
}

impl TempDir {
    /// A directory no other `TempDir` can be given, however coarse the clock.
    ///
    /// It used to be named from `SystemTime::now()` alone, and that is only as
    /// fine-grained as the platform's timer. Cargo runs these tests in parallel
    /// threads, so on macOS two of them were built within one tick, were handed
    /// the same directory, and wrote the same `model.gguf`: `fs::write`
    /// truncates before it writes, so one test read the other's file in the
    /// window where it was zero bytes long. It surfaced as
    /// `Truncated { offset: 0, needed: 4, available: 0 }` from the reader -
    /// a message about the fixture that was really about the harness.
    ///
    /// The counter settles it within a process and the pid across the several
    /// test binaries cargo runs at once. The clock stays for readability: it
    /// says when a leftover directory is from.
    pub fn new(tag: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(directory_name(tag, nanos));
        std::fs::create_dir_all(&path).expect("create the fixture directory");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Write `bytes` to a file in this directory and return its path.
    ///
    /// Loud on failure. A discarded error here meant a fixture that was never
    /// written came back as a parse error about its contents, which is how the
    /// collision above stayed hidden: every symptom pointed at the reader.
    pub fn write(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, bytes)
            .unwrap_or_else(|cause| panic!("write the fixture {}: {cause}", path.display()));
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// One instant, handed over twice, must still be two directories.
    ///
    /// Checked against the defect it exists for: name the directory from the
    /// clock reading alone and this fails everywhere, immediately. Testing it
    /// through the real clock instead would have proved nothing - on Linux the
    /// timer is fine enough that even the broken version passes, which is why
    /// the collision reached a macOS runner before anything caught it.
    #[test]
    fn one_instant_twice_is_still_two_directories() {
        let frozen = 1_700_000_000_000_000_000u128;
        let first = directory_name("collision", frozen);
        let second = directory_name("collision", frozen);
        assert_ne!(
            first, second,
            "a stopped clock collapsed two fixtures into one"
        );
    }

    /// And across threads, because cargo runs these tests on several of them.
    /// A per-thread counter would satisfy the test above and still collide.
    #[test]
    fn one_instant_across_threads_is_still_distinct_directories() {
        let frozen = 1_700_000_000_000_000_000u128;
        let names: Vec<String> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(move || {
                        (0..32)
                            .map(|_| directory_name("threaded", frozen))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().expect("worker"))
                .collect()
        });
        let distinct: HashSet<&String> = names.iter().collect();
        assert_eq!(
            distinct.len(),
            names.len(),
            "two threads shared a directory"
        );
    }

    /// The whole thing, end to end: real directories, really created, and no
    /// two of them the same.
    #[test]
    fn fixtures_created_together_do_not_share_a_directory() {
        let made: Vec<TempDir> = (0..64).map(|_| TempDir::new("together")).collect();
        let distinct: HashSet<&Path> = made.iter().map(TempDir::path).collect();
        assert_eq!(
            distinct.len(),
            made.len(),
            "two fixtures shared a directory"
        );
    }
}
