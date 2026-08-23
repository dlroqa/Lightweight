//! The GGUF header reader.
//!
//! Binary layout, transcribed from `ggml/src/gguf.cpp` and
//! `ggml/include/gguf.h` at build b10590:
//!
//! ```text
//! magic        [u8; 4]  == "GGUF"
//! version      u32       (this build accepts 2 and 3)
//! tensor_count u64
//! kv_count     u64
//! kv[kv_count]
//!     key      { len: u64, bytes: [u8; len] }   UTF-8, not NUL-terminated
//!     type     u32
//!     value    per type
//! tensor_info[tensor_count]
//!     name     { len: u64, bytes: [u8; len] }
//!     n_dims   u32                              at most GGML_MAX_DIMS (4)
//!     dims     [u64; n_dims]
//!     type     u32                              a ggml_type
//!     offset   u64                              relative to the data section
//! padding to `general.alignment` (default 32, only when tensors are present)
//! tensor data                                   NEVER READ BY US
//! ```
//!
//! Three properties this reader is built to guarantee, because spec section 27
//! says a malformed model must not crash the application:
//!
//! 1. **No panics.** Every read is bounds-checked against the real file length
//!    and every arithmetic operation on a length taken from the file is
//!    checked. `unwrap`, `panic` and slice indexing are denied at crate level.
//! 2. **Bounded memory.** A corrupt length field cannot make us allocate: reads
//!    are refused before they happen if they exceed what remains in the file or
//!    the configured limits.
//! 3. **No tensor data.** Only the header region is touched, so inspecting a
//!    4 GB model costs a few hundred kilobytes of reads.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use hermes_core::GgmlType;

use crate::error::GgufError;
use crate::value::{ArraySummary, GgufValue, GgufValueType, ensure_fits, fixed_array_byte_len};

/// File magic, from `GGUF_MAGIC` in `gguf.h`.
pub const MAGIC: [u8; 4] = *b"GGUF";
/// Oldest version this reader accepts. v1 used 32-bit counts and is rejected by
/// llama.cpp itself (`gguf.cpp:502`), so there is nothing to be gained by
/// accepting it here.
pub const MIN_VERSION: u32 = 2;
/// Newest version this reader accepts, from `GGUF_VERSION`.
pub const MAX_VERSION: u32 = 3;
/// From `GGUF_DEFAULT_ALIGNMENT`.
pub const DEFAULT_ALIGNMENT: u64 = 32;
/// From `GGML_MAX_DIMS`.
pub const MAX_DIMS: u32 = 4;

/// The key holding a file's tensor-data alignment.
pub const ALIGNMENT_KEY: &str = "general.alignment";

/// Smallest number of bytes a single key/value entry can occupy: an empty key
/// (8 bytes of length) plus a type tag (4). Used to reject an implausible
/// `kv_count` before looping on it.
const MIN_KV_BYTES: u64 = 12;
/// Smallest number of bytes one tensor-info entry can occupy: an empty name
/// (8), `n_dims` (4), a type (4) and an offset (8).
const MIN_TENSOR_BYTES: u64 = 24;

/// Caps that keep a corrupt file from turning into an allocation.
#[derive(Clone, Copy, Debug)]
pub struct ReadLimits {
    /// Ceiling on the whole header region. Real models sit far below this; a
    /// file claiming more is corrupt, and refusing early means we never try to
    /// buffer it.
    pub max_metadata_bytes: u64,
    /// Ceiling on any single string value. Chat templates are the largest
    /// legitimate strings and run to tens of kilobytes.
    pub max_string_bytes: u64,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_metadata_bytes: 64 * 1024 * 1024,
            max_string_bytes: 16 * 1024 * 1024,
        }
    }
}

/// One tensor's header entry. The tensor's *data* is never read.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TensorInfo {
    pub name: String,
    /// Extents. ggml defines `ne[dim]` as 1 for `dim >= n_dims`, so trailing
    /// entries are 1 and the element count is always the product of all four.
    pub dims: [u64; MAX_DIMS as usize],
    pub n_dims: u32,
    pub ggml_type: GgmlType,
    /// Offset within the tensor-data section.
    pub offset: u64,
}

impl TensorInfo {
    /// Total number of elements.
    pub fn elements(&self) -> Option<u64> {
        self.dims
            .iter()
            .try_fold(1u64, |acc, &dim| acc.checked_mul(dim))
    }

    /// Bytes this tensor occupies, from its type's block geometry.
    ///
    /// `None` when the element count overflows or the ggml type is one this
    /// build does not know — which is what makes an estimate "partial" rather
    /// than quietly too small.
    pub fn byte_size(&self) -> Option<u64> {
        self.ggml_type.bytes_for_elements(self.elements()?)
    }
}

/// A parsed GGUF header.
#[derive(Clone, Debug)]
pub struct GgufFile {
    path: PathBuf,
    version: u32,
    alignment: u64,
    metadata: BTreeMap<String, GgufValue>,
    tensors: Vec<TensorInfo>,
    data_offset: u64,
    file_size: u64,
}

impl GgufFile {
    /// Parse the header of the file at `path` with the default limits.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, GgufError> {
        Self::open_with_limits(path, ReadLimits::default())
    }

    /// Parse the header of a file whose tensor data is not present.
    ///
    /// Skips the check that the declared weights actually exist. Two callers
    /// need this and neither is a mistake: the download manager, which wants to
    /// show a model's metadata while the file is still arriving, and the test
    /// suite, which captures real models' headers with an HTTP range request
    /// rather than downloading gigabytes.
    ///
    /// Never use it on a file that is about to be loaded - that is precisely
    /// the case the check exists for.
    pub fn open_header_only(path: impl AsRef<Path>) -> Result<Self, GgufError> {
        Self::open_inner(path, ReadLimits::default(), false)
    }

    /// Parse the header, with explicit limits.
    pub fn open_with_limits(path: impl AsRef<Path>, limits: ReadLimits) -> Result<Self, GgufError> {
        Self::open_inner(path, limits, true)
    }

    fn open_inner(
        path: impl AsRef<Path>,
        limits: ReadLimits,
        verify_data: bool,
    ) -> Result<Self, GgufError> {
        let path = path.as_ref().to_path_buf();

        let file = File::open(&path).map_err(|source| GgufError::Open {
            path: path.clone(),
            source,
        })?;
        let file_size = file
            .metadata()
            .map_err(|source| GgufError::Open {
                path: path.clone(),
                source,
            })?
            .len();

        let mut cursor = Cursor {
            inner: BufReader::new(file),
            path: path.clone(),
            offset: 0,
            file_size,
            limits,
        };

        Self::parse(&mut cursor, path, file_size, verify_data)
    }

    /// Limits are enforced by the cursor, which already holds them.
    fn parse<R: Read + Seek>(
        cursor: &mut Cursor<R>,
        path: PathBuf,
        file_size: u64,
        verify_data: bool,
    ) -> Result<Self, GgufError> {
        let magic = cursor.read_array::<4>()?;
        if magic != MAGIC {
            return Err(GgufError::BadMagic { found: magic });
        }

        let version = cursor.read_u32()?;
        if !(MIN_VERSION..=MAX_VERSION).contains(&version) {
            return Err(GgufError::UnsupportedVersion {
                found: version,
                min: MIN_VERSION,
                max: MAX_VERSION,
            });
        }

        let tensor_count = cursor.read_u64()?;
        let kv_count = cursor.read_u64()?;

        // Reject counts that cannot possibly fit before allocating for them.
        // Without this, `kv_count = u64::MAX` would have us loop 1.8e19 times.
        ensure_fits(
            "metadata keys",
            kv_count.saturating_mul(MIN_KV_BYTES),
            cursor.remaining(),
            cursor.offset,
        )?;

        let mut metadata = BTreeMap::new();
        for _ in 0..kv_count {
            let (key, value) = cursor.read_kv()?;
            cursor.check_metadata_budget()?;
            metadata.insert(key, value);
        }

        let alignment = Self::resolve_alignment(&metadata)?;

        ensure_fits(
            "tensors",
            tensor_count.saturating_mul(MIN_TENSOR_BYTES),
            cursor.remaining(),
            cursor.offset,
        )?;

        // Reserve conservatively: `tensor_count` comes from the file, and the
        // check above only proves the entries could fit, not that they do.
        let mut tensors = Vec::with_capacity(usize::try_from(tensor_count.min(4096)).unwrap_or(0));
        for _ in 0..tensor_count {
            tensors.push(cursor.read_tensor_info()?);
            cursor.check_metadata_budget()?;
        }

        // gguf.h: the data section is padded to the alignment "if and only if
        // the gguf_context contains at least one tensor".
        let data_offset = if tensors.is_empty() {
            cursor.offset
        } else {
            align_up(cursor.offset, alignment)
        };

        let file = Self {
            path,
            version,
            alignment,
            metadata,
            tensors,
            data_offset,
            file_size,
        };
        if verify_data {
            file.verify_tensor_data_present()?;
        }
        Ok(file)
    }

    /// Check that the tensor data the header declares is actually in the file.
    ///
    /// Parsing only the header means a download interrupted *after* the header
    /// but partway through the weights would otherwise look perfectly valid —
    /// and the failure would surface much later, as an opaque engine crash on
    /// load. Spec section 27 wants that reported as an actionable "the file
    /// looks incomplete" instead, so it is caught here.
    ///
    /// Tensors whose ggml type this build cannot size are skipped rather than
    /// guessed at: an unknown type makes the check impossible, not failed.
    fn verify_tensor_data_present(&self) -> Result<(), GgufError> {
        for tensor in &self.tensors {
            let Some(size) = tensor.byte_size() else {
                continue;
            };
            let end = self
                .data_offset
                .saturating_add(tensor.offset)
                .saturating_add(size);
            if end > self.file_size {
                return Err(GgufError::Truncated {
                    offset: self.data_offset.saturating_add(tensor.offset),
                    needed: size,
                    available: self
                        .file_size
                        .saturating_sub(self.data_offset.saturating_add(tensor.offset)),
                });
            }
        }
        Ok(())
    }

    /// Read `general.alignment`, defaulting and validating as llama.cpp does.
    ///
    /// `gguf.cpp:623` rejects a zero or non-power-of-two alignment. We do the
    /// same, because `align_up` with a non-power-of-two would silently compute
    /// the wrong data offset.
    fn resolve_alignment(metadata: &BTreeMap<String, GgufValue>) -> Result<u64, GgufError> {
        let Some(value) = metadata.get(ALIGNMENT_KEY) else {
            return Ok(DEFAULT_ALIGNMENT);
        };
        let alignment = value.as_u64().unwrap_or(0);
        if alignment == 0 || !alignment.is_power_of_two() {
            return Err(GgufError::ImplausibleCount {
                what: "byte alignment (must be a power of two)",
                claimed: alignment,
                available: DEFAULT_ALIGNMENT,
                offset: 0,
            });
        }
        Ok(alignment)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn version(&self) -> u32 {
        self.version
    }

    pub const fn alignment(&self) -> u64 {
        self.alignment
    }

    /// Every metadata key/value. Arrays are summaries, not contents.
    pub const fn metadata(&self) -> &BTreeMap<String, GgufValue> {
        &self.metadata
    }

    pub fn tensors(&self) -> &[TensorInfo] {
        &self.tensors
    }

    /// Offset at which tensor data begins.
    pub const fn data_offset(&self) -> u64 {
        self.data_offset
    }

    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    /// Bytes the tensor data occupies on disk, from the file's own length.
    ///
    /// Useful as a cross-check on the sum of per-tensor sizes: the two should
    /// agree, and a mismatch means either a truncated file or a ggml type we
    /// mis-sized.
    pub const fn tensor_data_bytes(&self) -> u64 {
        self.file_size.saturating_sub(self.data_offset)
    }

    pub fn get(&self, key: &str) -> Option<&GgufValue> {
        self.metadata.get(key)
    }

    pub fn get_u64(&self, key: &str) -> Option<u64> {
        self.get(key)?.as_u64()
    }

    pub fn get_u32(&self, key: &str) -> Option<u32> {
        self.get(key)?.as_u32()
    }

    pub fn get_f64(&self, key: &str) -> Option<f64> {
        self.get(key)?.as_f64()
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.get(key)?.as_bool()
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key)?.as_str()
    }

    pub fn get_array(&self, key: &str) -> Option<&ArraySummary> {
        self.get(key)?.as_array()
    }

    /// Total elements across all tensors: the model's parameter count.
    ///
    /// `None` if any tensor's extents overflow, rather than a wrong total.
    pub fn parameter_count(&self) -> Option<u64> {
        self.tensors
            .iter()
            .try_fold(0u64, |acc, tensor| acc.checked_add(tensor.elements()?))
    }

    /// Exact bytes the weights occupy.
    ///
    /// `None` if any tensor uses a ggml type this build cannot size. Returning
    /// a partial sum would understate the model and could turn an INSUFFICIENT
    /// RAM verdict into SAFE, so the failure is explicit.
    pub fn weight_bytes(&self) -> Option<u64> {
        self.tensors
            .iter()
            .try_fold(0u64, |acc, tensor| acc.checked_add(tensor.byte_size()?))
    }

    /// Read the elements of an integer array.
    ///
    /// Opt-in, because array contents are deliberately not held in memory. The
    /// caller that needs this is the KV-cache estimator, for architectures that
    /// write `attention.head_count_kv` per layer rather than as one scalar.
    pub fn read_u64_array(&self, key: &str) -> Result<Option<Vec<u64>>, GgufError> {
        let Some(summary) = self.get_array(key) else {
            // A scalar in place of a one-element array is normal and not an
            // error: most architectures write a single head count.
            return Ok(self.get_u64(key).map(|value| vec![value]));
        };

        let Some(width) = summary.element_type.fixed_width() else {
            return Ok(None);
        };

        let len = usize::try_from(summary.len).unwrap_or(usize::MAX);
        let file = File::open(&self.path).map_err(|source| GgufError::Open {
            path: self.path.clone(),
            source,
        })?;
        let mut reader = BufReader::new(file);
        reader
            .seek(SeekFrom::Start(summary.offset))
            .map_err(|source| GgufError::Read {
                path: self.path.clone(),
                offset: summary.offset,
                source,
            })?;

        let mut out = Vec::with_capacity(len.min(1 << 20));
        let mut buffer = [0u8; 8];
        for index in 0..len {
            let width_usize = usize::try_from(width).unwrap_or(8);
            let slot = buffer.get_mut(..width_usize).ok_or(GgufError::Truncated {
                offset: summary.offset,
                needed: width,
                available: 8,
            })?;
            reader.read_exact(slot).map_err(|source| GgufError::Read {
                path: self.path.clone(),
                offset: summary
                    .offset
                    .saturating_add((index as u64).saturating_mul(width)),
                source,
            })?;
            out.push(decode_unsigned(slot));
        }
        Ok(Some(out))
    }
}

/// Little-endian decode of 1, 2, 4 or 8 bytes into a `u64`.
///
/// Zero-extends into a fixed buffer rather than shifting, so there is no shift
/// width to get wrong and nothing to overflow.
fn decode_unsigned(bytes: &[u8]) -> u64 {
    let mut padded = [0u8; 8];
    for (slot, byte) in padded.iter_mut().zip(bytes) {
        *slot = *byte;
    }
    u64::from_le_bytes(padded)
}

/// Round `offset` up to a multiple of `alignment`.
///
/// `alignment` is validated to be a non-zero power of two before this is
/// reached, so the arithmetic cannot divide by zero.
const fn align_up(offset: u64, alignment: u64) -> u64 {
    match offset.checked_rem(alignment) {
        // A zero alignment is rejected during parsing, but `checked_rem`
        // expresses that here rather than relying on the caller.
        None | Some(0) => offset,
        Some(remainder) => offset.saturating_add(alignment.saturating_sub(remainder)),
    }
}

/// A bounds-checked, offset-tracking cursor over the header region.
struct Cursor<R> {
    inner: R,
    path: PathBuf,
    offset: u64,
    file_size: u64,
    limits: ReadLimits,
}

impl<R: Read + Seek> Cursor<R> {
    /// Bytes left in the file from the current position.
    const fn remaining(&self) -> u64 {
        self.file_size.saturating_sub(self.offset)
    }

    /// Refuse a read that runs past the end of the file, before attempting it.
    const fn need(&self, bytes: u64) -> Result<(), GgufError> {
        if bytes > self.remaining() {
            return Err(GgufError::Truncated {
                offset: self.offset,
                needed: bytes,
                available: self.remaining(),
            });
        }
        Ok(())
    }

    fn check_metadata_budget(&self) -> Result<(), GgufError> {
        if self.offset > self.limits.max_metadata_bytes {
            return Err(GgufError::MetadataTooLarge {
                claimed: self.offset,
                limit: self.limits.max_metadata_bytes,
            });
        }
        Ok(())
    }

    fn read_array<const N: usize>(&mut self) -> Result<[u8; N], GgufError> {
        self.need(N as u64)?;
        let mut buffer = [0u8; N];
        self.inner
            .read_exact(&mut buffer)
            .map_err(|source| GgufError::Read {
                path: self.path.clone(),
                offset: self.offset,
                source,
            })?;
        self.offset = self.offset.saturating_add(N as u64);
        Ok(buffer)
    }

    fn read_bytes(&mut self, len: u64) -> Result<Vec<u8>, GgufError> {
        self.need(len)?;
        let len_usize = usize::try_from(len).map_err(|_| GgufError::ImplausibleCount {
            what: "string bytes",
            claimed: len,
            available: self.remaining(),
            offset: self.offset,
        })?;
        // Safe to allocate: `need` proved the file really holds this many bytes.
        let mut buffer = vec![0u8; len_usize];
        self.inner
            .read_exact(&mut buffer)
            .map_err(|source| GgufError::Read {
                path: self.path.clone(),
                offset: self.offset,
                source,
            })?;
        self.offset = self.offset.saturating_add(len);
        Ok(buffer)
    }

    fn skip(&mut self, len: u64) -> Result<(), GgufError> {
        self.need(len)?;
        let signed = i64::try_from(len).map_err(|_| GgufError::ImplausibleCount {
            what: "bytes to skip",
            claimed: len,
            available: self.remaining(),
            offset: self.offset,
        })?;
        self.inner
            .seek_relative(signed)
            .map_err(|source| GgufError::Read {
                path: self.path.clone(),
                offset: self.offset,
                source,
            })?;
        self.offset = self.offset.saturating_add(len);
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8, GgufError> {
        Ok(u8::from_le_bytes(self.read_array::<1>()?))
    }

    fn read_u16(&mut self) -> Result<u16, GgufError> {
        Ok(u16::from_le_bytes(self.read_array::<2>()?))
    }

    fn read_u32(&mut self) -> Result<u32, GgufError> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    fn read_u64(&mut self) -> Result<u64, GgufError> {
        Ok(u64::from_le_bytes(self.read_array::<8>()?))
    }

    /// A length-prefixed UTF-8 string.
    fn read_string(&mut self, context: &str) -> Result<String, GgufError> {
        let len = self.read_u64()?;
        if len > self.limits.max_string_bytes {
            return Err(GgufError::ImplausibleCount {
                what: "string bytes",
                claimed: len,
                available: self.limits.max_string_bytes,
                offset: self.offset,
            });
        }
        let offset = self.offset;
        let bytes = self.read_bytes(len)?;
        String::from_utf8(bytes).map_err(|_| GgufError::InvalidUtf8 {
            key: context.to_owned(),
            offset,
        })
    }

    fn read_kv(&mut self) -> Result<(String, GgufValue), GgufError> {
        let key = self.read_string("<key>")?;
        let tag_offset = self.offset;
        let tag = self.read_u32()?;
        let value_type =
            GgufValueType::from_tag(tag).ok_or_else(|| GgufError::UnknownValueType {
                key: key.clone(),
                value_type: tag,
                offset: tag_offset,
            })?;
        let value = self.read_value(&key, value_type)?;
        Ok((key, value))
    }

    fn read_value(&mut self, key: &str, value_type: GgufValueType) -> Result<GgufValue, GgufError> {
        Ok(match value_type {
            GgufValueType::U8 => GgufValue::U8(self.read_u8()?),
            GgufValueType::I8 => GgufValue::I8(self.read_u8()? as i8),
            GgufValueType::U16 => GgufValue::U16(self.read_u16()?),
            GgufValueType::I16 => GgufValue::I16(self.read_u16()? as i16),
            GgufValueType::U32 => GgufValue::U32(self.read_u32()?),
            GgufValueType::I32 => GgufValue::I32(self.read_u32()? as i32),
            GgufValueType::U64 => GgufValue::U64(self.read_u64()?),
            GgufValueType::I64 => GgufValue::I64(self.read_u64()? as i64),
            GgufValueType::F32 => GgufValue::F32(f32::from_bits(self.read_u32()?)),
            GgufValueType::F64 => GgufValue::F64(f64::from_bits(self.read_u64()?)),
            // gguf.h stores bools as int8. Anything non-zero is true.
            GgufValueType::Bool => GgufValue::Bool(self.read_u8()? != 0),
            GgufValueType::String => GgufValue::String(self.read_string(key)?),
            GgufValueType::Array => GgufValue::Array(self.read_array_summary(key)?),
        })
    }

    /// Record an array's shape and skip its contents.
    fn read_array_summary(&mut self, key: &str) -> Result<ArraySummary, GgufError> {
        let type_offset = self.offset;
        let tag = self.read_u32()?;
        let element_type =
            GgufValueType::from_tag(tag).ok_or_else(|| GgufError::UnknownValueType {
                key: key.to_owned(),
                value_type: tag,
                offset: type_offset,
            })?;

        if element_type == GgufValueType::Array {
            // The format has no representation for this, and llama.cpp does not
            // produce it. Treating it as an error beats attempting to recurse.
            return Err(GgufError::NestedArray {
                key: key.to_owned(),
                offset: type_offset,
            });
        }

        let len = self.read_u64()?;
        let offset = self.offset;

        let byte_len = if element_type == GgufValueType::String {
            // String elements are individually length-prefixed, so the only way
            // to learn the total size is to walk them. We still never hold the
            // contents: each is skipped, not read. This is the path a 150,000
            // entry tokenizer vocabulary takes, and it costs a few milliseconds
            // rather than tens of megabytes.
            ensure_fits(
                "array elements",
                len.saturating_mul(8),
                self.remaining(),
                self.offset,
            )?;
            let start = self.offset;
            for _ in 0..len {
                let element_len = self.read_u64()?;
                if element_len > self.limits.max_string_bytes {
                    return Err(GgufError::ImplausibleCount {
                        what: "string bytes",
                        claimed: element_len,
                        available: self.limits.max_string_bytes,
                        offset: self.offset,
                    });
                }
                self.skip(element_len)?;
            }
            self.offset.saturating_sub(start)
        } else {
            let byte_len =
                fixed_array_byte_len(element_type, len).ok_or(GgufError::ImplausibleCount {
                    what: "array elements",
                    claimed: len,
                    available: self.remaining(),
                    offset: self.offset,
                })?;
            self.skip(byte_len)?;
            byte_len
        };

        Ok(ArraySummary {
            element_type,
            len,
            offset,
            byte_len,
        })
    }

    fn read_tensor_info(&mut self) -> Result<TensorInfo, GgufError> {
        let name = self.read_string("<tensor name>")?;
        let n_dims = self.read_u32()?;
        if n_dims > MAX_DIMS {
            return Err(GgufError::TooManyDimensions {
                name,
                dims: n_dims,
                limit: MAX_DIMS,
            });
        }

        // ggml defines ne[dim] as 1 for dim >= n_dims, so unread extents are 1
        // and the element count is the product of the whole array.
        let mut dims = [1u64; MAX_DIMS as usize];
        for index in 0..n_dims as usize {
            let extent = self.read_u64()?;
            match dims.get_mut(index) {
                Some(slot) => *slot = extent,
                // Unreachable given the bound above, but the crate denies
                // indexing, and an explicit error beats a silent truncation.
                None => {
                    return Err(GgufError::TooManyDimensions {
                        name,
                        dims: n_dims,
                        limit: MAX_DIMS,
                    });
                }
            }
        }

        let ggml_type = GgmlType::from_id(self.read_u32()?);
        let offset = self.read_u64()?;

        let info = TensorInfo {
            name,
            dims,
            n_dims,
            ggml_type,
            offset,
        };

        if info.elements().is_none() {
            return Err(GgufError::DimensionOverflow {
                name: info.name.clone(),
            });
        }
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{FixtureValue, GgufBuilder, TempDir};

    fn open_bytes(tag: &str, bytes: &[u8]) -> (TempDir, Result<GgufFile, GgufError>) {
        let dir = TempDir::new(tag);
        let path = dir.write("model.gguf", bytes);
        let result = GgufFile::open(&path);
        (dir, result)
    }

    #[test]
    fn round_trips_a_complete_model() {
        // The writer and the reader were written independently from the same
        // spec; this asserts they agree.
        let bytes = GgufBuilder::small_model("llama").build();
        let (_dir, file) = open_bytes("roundtrip", &bytes);
        let file = file.expect("fixture should parse");

        assert_eq!(file.version(), MAX_VERSION);
        assert_eq!(file.alignment(), DEFAULT_ALIGNMENT);
        assert_eq!(file.get_str("general.architecture"), Some("llama"));
        assert_eq!(file.get_u64("llama.block_count"), Some(2));
        assert_eq!(file.get_u64("llama.attention.head_count_kv"), Some(2));
        assert_eq!(file.get_f64("llama.rope.freq_base"), Some(10_000.0));
        assert_eq!(file.tensors().len(), 4);
    }

    #[test]
    fn computes_parameter_count_and_weight_bytes_exactly() {
        let bytes = GgufBuilder::small_model("llama").build();
        let (_dir, file) = open_bytes("sizes", &bytes);
        let file = file.expect("parse");

        // 64*128 + 64*64 + 64*64 + 64
        let expected_params = 64 * 128 + 64 * 64 + 64 * 64 + 64;
        assert_eq!(file.parameter_count(), Some(expected_params));

        // q4_K is 144 bytes per 256 elements; f32 is 4 bytes per element.
        let expected_bytes =
            (8192u64 / 256) * 144 + (4096u64 / 256) * 144 + (4096u64 / 256) * 144 + 64 * 4;
        assert_eq!(file.weight_bytes(), Some(expected_bytes));
    }

    #[test]
    fn weight_bytes_agree_with_the_data_section_on_disk() {
        // An independent cross-check of the ggml type table: the sum of
        // per-tensor sizes must equal the bytes actually present after the
        // header. If the block geometry were wrong, these would diverge.
        let bytes = GgufBuilder::small_model("qwen3").build();
        let (_dir, file) = open_bytes("datasection", &bytes);
        let file = file.expect("parse");
        assert_eq!(file.weight_bytes(), Some(file.tensor_data_bytes()));
    }

    #[test]
    fn a_large_vocabulary_is_summarized_not_materialized() {
        // The point of summarizing arrays: learning the vocabulary size must
        // not cost the memory of holding the vocabulary.
        let vocab: Vec<String> = (0..50_000).map(|i| format!("token{i}")).collect();
        let bytes = GgufBuilder::small_model("llama")
            .kv("tokenizer.ggml.tokens", FixtureValue::StrArray(vocab))
            .build();
        let (_dir, file) = open_bytes("vocab", &bytes);
        let file = file.expect("parse");

        let summary = file
            .get_array("tokenizer.ggml.tokens")
            .expect("array summary");
        assert_eq!(summary.len, 50_000);
        assert_eq!(summary.element_type, GgufValueType::String);
        assert!(summary.byte_len > 0);
    }

    #[test]
    fn reads_a_per_layer_head_count_array_on_request() {
        // Some architectures write attention.head_count_kv per layer. The
        // KV-cache estimate has to handle both shapes without knowing which
        // architecture it is looking at.
        let bytes = GgufBuilder::small_model("lfm2")
            .kv(
                "lfm2.attention.head_count_kv",
                FixtureValue::U32Array(vec![0, 8, 0, 8]),
            )
            .build();
        let (_dir, file) = open_bytes("perlayer", &bytes);
        let file = file.expect("parse");

        let heads = file
            .read_u64_array("lfm2.attention.head_count_kv")
            .expect("read array")
            .expect("present");
        assert_eq!(heads, vec![0, 8, 0, 8]);
    }

    #[test]
    fn a_scalar_reads_as_a_one_element_sequence() {
        // So callers need no branch on which shape the file used.
        let bytes = GgufBuilder::small_model("llama").build();
        let (_dir, file) = open_bytes("scalarseq", &bytes);
        let file = file.expect("parse");
        assert_eq!(
            file.read_u64_array("llama.attention.head_count_kv")
                .expect("read"),
            Some(vec![2])
        );
    }

    // ---- corruption: every case must return Err, never panic ----

    #[test]
    fn rejects_a_file_that_is_not_gguf() {
        let (_dir, result) = open_bytes("magic", b"NOTAGGUFFILE................");
        assert!(matches!(result, Err(GgufError::BadMagic { .. })));
    }

    #[test]
    fn rejects_an_unsupported_version() {
        let bytes = GgufBuilder::small_model("llama").version(1).build();
        let (_dir, result) = open_bytes("v1", &bytes);
        assert!(matches!(
            result,
            Err(GgufError::UnsupportedVersion { found: 1, .. })
        ));

        let bytes = GgufBuilder::small_model("llama").version(99).build();
        let (_dir, result) = open_bytes("v99", &bytes);
        assert!(matches!(
            result,
            Err(GgufError::UnsupportedVersion { found: 99, .. })
        ));
    }

    #[test]
    fn truncation_at_every_boundary_is_an_error_never_a_panic() {
        // The single most valuable test here: an interrupted download can end
        // at any offset, and none of those may crash the application.
        let full = GgufBuilder::small_model("llama").build();
        for cut in (0..full.len().min(3_000)).step_by(7) {
            let (_dir, result) = open_bytes("truncate", &full[..cut]);
            assert!(
                result.is_err(),
                "a file truncated to {cut} bytes parsed successfully"
            );
        }
    }

    #[test]
    fn an_absurd_key_count_is_rejected_before_allocating() {
        let mut bytes = GgufBuilder::small_model("llama").build();
        // kv_count sits at offset 16, right after magic, version, tensor_count.
        bytes[16..24].copy_from_slice(&u64::MAX.to_le_bytes());
        let (_dir, result) = open_bytes("kvcount", &bytes);
        assert!(matches!(result, Err(GgufError::ImplausibleCount { .. })));
    }

    #[test]
    fn an_absurd_tensor_count_is_rejected_before_allocating() {
        let mut bytes = GgufBuilder::small_model("llama").build();
        bytes[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        let (_dir, result) = open_bytes("tcount", &bytes);
        assert!(result.is_err());
    }

    #[test]
    fn a_string_length_past_the_end_of_the_file_is_rejected() {
        let mut bytes = GgufBuilder::small_model("llama").build();
        // The first key's length prefix begins at offset 24.
        bytes[24..32].copy_from_slice(&(1u64 << 40).to_le_bytes());
        let (_dir, result) = open_bytes("strlen", &bytes);
        assert!(result.is_err());
    }

    #[test]
    fn an_unknown_value_type_tag_is_rejected() {
        // "general.architecture" is 20 bytes, so its type tag sits at 24+8+20.
        let mut bytes = GgufBuilder::small_model("llama").build();
        let tag_offset = 24 + 8 + "general.architecture".len();
        bytes[tag_offset..tag_offset + 4].copy_from_slice(&77u32.to_le_bytes());
        let (_dir, result) = open_bytes("badtag", &bytes);
        assert!(matches!(
            result,
            Err(GgufError::UnknownValueType { value_type: 77, .. })
        ));
    }

    #[test]
    fn a_non_power_of_two_alignment_is_rejected() {
        // llama.cpp rejects this at gguf.cpp:623. Accepting it would make our
        // data-offset arithmetic silently wrong.
        let bytes = GgufBuilder::small_model("llama").alignment(24).build();
        let (_dir, result) = open_bytes("align", &bytes);
        assert!(matches!(result, Err(GgufError::ImplausibleCount { .. })));
    }

    #[test]
    fn a_zero_alignment_is_rejected_rather_than_dividing_by_zero() {
        let bytes = GgufBuilder::small_model("llama").alignment(0).build();
        let (_dir, result) = open_bytes("align0", &bytes);
        assert!(matches!(result, Err(GgufError::ImplausibleCount { .. })));
    }

    #[test]
    fn a_metadata_budget_smaller_than_the_file_is_enforced() {
        let dir = TempDir::new("budget");
        let path = dir.write("model.gguf", &GgufBuilder::small_model("llama").build());
        let result = GgufFile::open_with_limits(
            &path,
            ReadLimits {
                max_metadata_bytes: 16,
                max_string_bytes: 1024,
            },
        );
        assert!(matches!(result, Err(GgufError::MetadataTooLarge { .. })));
    }

    #[test]
    fn errors_carry_the_offset_they_were_found_at() {
        let full = GgufBuilder::small_model("llama").build();
        let (_dir, result) = open_bytes("offset", &full[..40]);
        let err = result.expect_err("truncated file");
        assert!(err.offset().is_some(), "no offset on {err}");
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let result = GgufFile::open("/nonexistent/definitely/not/here.gguf");
        assert!(matches!(result, Err(GgufError::Open { .. })));
    }

    #[test]
    fn an_empty_file_is_an_error_not_a_panic() {
        let (_dir, result) = open_bytes("empty", b"");
        assert!(result.is_err());
    }

    // ---- offsets ----

    #[test]
    fn data_offset_is_aligned_and_within_the_file() {
        let bytes = GgufBuilder::small_model("llama").build();
        let (_dir, file) = open_bytes("offsets", &bytes);
        let file = file.expect("parse");

        assert_eq!(file.data_offset() % file.alignment(), 0);
        assert!(file.data_offset() <= file.file_size());
    }

    #[test]
    fn a_header_only_file_needs_no_padding() {
        // gguf.h: padding applies "if and only if the context contains at least
        // one tensor".
        let bytes = GgufBuilder::new()
            .kv("general.architecture", "llama")
            .build();
        let (_dir, file) = open_bytes("notensors", &bytes);
        let file = file.expect("parse");
        assert_eq!(file.tensors().len(), 0);
        assert_eq!(file.data_offset(), file.file_size());
    }

    #[test]
    fn align_up_rounds_to_the_next_multiple() {
        assert_eq!(align_up(0, 32), 0);
        assert_eq!(align_up(1, 32), 32);
        assert_eq!(align_up(32, 32), 32);
        assert_eq!(align_up(33, 32), 64);
    }

    #[test]
    fn trailing_dimensions_default_to_one() {
        // ggml defines ne[dim] as 1 for dim >= n_dims, so a 1-D tensor's
        // element count is its single extent, not zero.
        let bytes = GgufBuilder::new()
            .kv("general.architecture", "llama")
            .tensor("norm.weight", &[64], GgmlType::F32)
            .build();
        let (_dir, file) = open_bytes("dims", &bytes);
        let file = file.expect("parse");
        let tensor = &file.tensors()[0];
        assert_eq!(tensor.n_dims, 1);
        assert_eq!(tensor.dims, [64, 1, 1, 1]);
        assert_eq!(tensor.elements(), Some(64));
        assert_eq!(tensor.byte_size(), Some(256));
    }

    #[test]
    fn an_unknown_tensor_type_makes_the_total_unsizeable_not_wrong() {
        let bytes = GgufBuilder::new()
            .kv("general.architecture", "llama")
            .tensor("w", &[64], GgmlType::Unknown(9999))
            .build();
        let (_dir, file) = open_bytes("unknowntype", &bytes);
        let file = file.expect("parse");
        // Parameter count is still knowable from the extents.
        assert_eq!(file.parameter_count(), Some(64));
        // Byte size is not, and must not silently become zero.
        assert_eq!(file.weight_bytes(), None);
    }
}

#[cfg(test)]
mod header_only_tests {
    use super::*;
    use crate::fixture::{GgufBuilder, TempDir};

    #[test]
    fn header_only_mode_accepts_a_file_missing_its_weights() {
        // What a range request or a partial download looks like: a complete
        // header followed by nothing.
        let full = GgufBuilder::small_model("llama").build();
        let header_end = full.len() - 1;
        let dir = TempDir::new("headeronly");
        let path = dir.write("partial.gguf", &full[..header_end]);

        assert!(
            GgufFile::open(&path).is_err(),
            "the normal path must still reject an incomplete file"
        );
        let file = GgufFile::open_header_only(&path).expect("header-only parse");
        assert_eq!(file.get_str("general.architecture"), Some("llama"));
    }
}
