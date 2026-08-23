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

impl TempDir {
    pub fn new(tag: &str) -> Self {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("hermes-gguf-{tag}-{unique}"));
        let _ = std::fs::create_dir_all(&path);
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Write `bytes` to a file in this directory and return its path.
    pub fn write(&self, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = self.0.join(name);
        let _ = std::fs::write(&path, bytes);
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
