//! GGUF metadata values.
//!
//! The value-type numbering is transcribed from `enum gguf_type` in
//! `ggml/include/gguf.h` at build b10590.
//!
//! Arrays are deliberately *not* materialized. A tokenizer vocabulary is an
//! array of 150,000 strings; reading one to learn its length would cost tens of
//! megabytes for a number that is written in its header. [`ArraySummary`]
//! records the shape and where the elements are, and [`crate::GgufFile`] offers
//! an explicit opt-in read for the rare caller that wants the contents.

use serde::{Deserialize, Serialize};

use crate::error::GgufError;

/// A GGUF metadata value type tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GgufValueType {
    U8,
    I8,
    U16,
    I16,
    U32,
    I32,
    F32,
    Bool,
    String,
    Array,
    U64,
    I64,
    F64,
}

impl GgufValueType {
    /// Map the on-disk tag.
    pub const fn from_tag(tag: u32) -> Option<Self> {
        match tag {
            0 => Some(Self::U8),
            1 => Some(Self::I8),
            2 => Some(Self::U16),
            3 => Some(Self::I16),
            4 => Some(Self::U32),
            5 => Some(Self::I32),
            6 => Some(Self::F32),
            7 => Some(Self::Bool),
            8 => Some(Self::String),
            9 => Some(Self::Array),
            10 => Some(Self::U64),
            11 => Some(Self::I64),
            12 => Some(Self::F64),
            _ => None,
        }
    }

    /// Encoded width in bytes, for the types that have a fixed one.
    ///
    /// `String` and `Array` are length-prefixed and so return `None`. Note that
    /// `Bool` is one byte: `gguf.h` states that bool arrays "are always stored
    /// as int8 on all platforms".
    pub const fn fixed_width(self) -> Option<u64> {
        match self {
            Self::U8 | Self::I8 | Self::Bool => Some(1),
            Self::U16 | Self::I16 => Some(2),
            Self::U32 | Self::I32 | Self::F32 => Some(4),
            Self::U64 | Self::I64 | Self::F64 => Some(8),
            Self::String | Self::Array => None,
        }
    }
}

/// Where an array lives, and what shape it is, without its contents.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArraySummary {
    pub element_type: GgufValueType,
    /// Number of elements, read from the array header.
    pub len: u64,
    /// File offset of the first element.
    pub offset: u64,
    /// Total bytes occupied by the elements.
    pub byte_len: u64,
}

/// A metadata value.
///
/// Scalars are held inline. Arrays are held as an [`ArraySummary`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum GgufValue {
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
    String(String),
    Array(ArraySummary),
}

impl GgufValue {
    pub const fn value_type(&self) -> GgufValueType {
        match self {
            Self::U8(_) => GgufValueType::U8,
            Self::I8(_) => GgufValueType::I8,
            Self::U16(_) => GgufValueType::U16,
            Self::I16(_) => GgufValueType::I16,
            Self::U32(_) => GgufValueType::U32,
            Self::I32(_) => GgufValueType::I32,
            Self::U64(_) => GgufValueType::U64,
            Self::I64(_) => GgufValueType::I64,
            Self::F32(_) => GgufValueType::F32,
            Self::F64(_) => GgufValueType::F64,
            Self::Bool(_) => GgufValueType::Bool,
            Self::String(_) => GgufValueType::String,
            Self::Array(_) => GgufValueType::Array,
        }
    }

    /// Read as an unsigned integer, accepting any integer width.
    ///
    /// GGUF writers disagree about width: the same logical key is a `u32` in
    /// one exporter and a `u64` in another, and llama.cpp's own reader coerces.
    /// Demanding an exact type here would reject perfectly good models for a
    /// reason the user could do nothing about. Negative signed values return
    /// `None` rather than wrapping — a negative layer count is corruption, not
    /// a large one.
    pub const fn as_u64(&self) -> Option<u64> {
        match *self {
            Self::U8(v) => Some(v as u64),
            Self::U16(v) => Some(v as u64),
            Self::U32(v) => Some(v as u64),
            Self::U64(v) => Some(v),
            Self::I8(v) if v >= 0 => Some(v as u64),
            Self::I16(v) if v >= 0 => Some(v as u64),
            Self::I32(v) if v >= 0 => Some(v as u64),
            Self::I64(v) if v >= 0 => Some(v as u64),
            _ => None,
        }
    }

    /// Read as an unsigned 32-bit integer, rejecting values that do not fit.
    pub fn as_u32(&self) -> Option<u32> {
        u32::try_from(self.as_u64()?).ok()
    }

    /// Read as a float, accepting either float width.
    pub const fn as_f64(&self) -> Option<f64> {
        match *self {
            Self::F32(v) => Some(v as f64),
            Self::F64(v) => Some(v),
            _ => None,
        }
    }

    pub const fn as_bool(&self) -> Option<bool> {
        match *self {
            Self::Bool(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(v) => Some(v),
            _ => None,
        }
    }

    pub const fn as_array(&self) -> Option<&ArraySummary> {
        match self {
            Self::Array(summary) => Some(summary),
            _ => None,
        }
    }

    /// Read a sequence of unsigned integers.
    ///
    /// A single scalar counts as a one-element sequence. This exists because
    /// some architectures write per-layer values: `attention.head_count_kv` is
    /// a scalar for most models but an array for hybrid ones, and the KV-cache
    /// estimate has to handle both without an architecture-specific branch.
    /// Array *contents* are not held in memory, so callers wanting the elements
    /// use [`crate::GgufFile::read_u64_array`]; this method only covers the
    /// scalar case.
    pub const fn as_scalar_sequence(&self) -> Option<u64> {
        self.as_u64()
    }
}

/// Elements of a fixed-width array occupy `len * width` bytes.
pub(crate) fn fixed_array_byte_len(element_type: GgufValueType, len: u64) -> Option<u64> {
    element_type.fixed_width()?.checked_mul(len)
}

/// Guard against a length field that would have us allocate absurdly.
pub(crate) fn ensure_fits(
    what: &'static str,
    claimed: u64,
    available: u64,
    offset: u64,
) -> Result<(), GgufError> {
    if claimed > available {
        return Err(GgufError::ImplausibleCount {
            what,
            claimed,
            available,
            offset,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_type_tags_match_the_gguf_header() {
        // Transcribed from `enum gguf_type` in ggml/include/gguf.h at b10590.
        let expected = [
            (0, GgufValueType::U8),
            (1, GgufValueType::I8),
            (2, GgufValueType::U16),
            (3, GgufValueType::I16),
            (4, GgufValueType::U32),
            (5, GgufValueType::I32),
            (6, GgufValueType::F32),
            (7, GgufValueType::Bool),
            (8, GgufValueType::String),
            (9, GgufValueType::Array),
            (10, GgufValueType::U64),
            (11, GgufValueType::I64),
            (12, GgufValueType::F64),
        ];
        for (tag, ty) in expected {
            assert_eq!(GgufValueType::from_tag(tag), Some(ty), "tag {tag}");
        }
        assert_eq!(GgufValueType::from_tag(13), None);
    }

    #[test]
    fn bool_is_one_byte_wide() {
        // gguf.h: bool arrays "are always stored as int8 on all platforms".
        // Assuming 4 bytes here would misplace every element after one.
        assert_eq!(GgufValueType::Bool.fixed_width(), Some(1));
    }

    #[test]
    fn variable_width_types_have_no_fixed_width() {
        assert_eq!(GgufValueType::String.fixed_width(), None);
        assert_eq!(GgufValueType::Array.fixed_width(), None);
    }

    #[test]
    fn integers_coerce_across_widths() {
        // The same key is u32 in one exporter and u64 in another.
        assert_eq!(GgufValue::U32(32).as_u64(), Some(32));
        assert_eq!(GgufValue::U64(32).as_u64(), Some(32));
        assert_eq!(GgufValue::I32(32).as_u64(), Some(32));
        assert_eq!(GgufValue::U8(32).as_u64(), Some(32));
    }

    #[test]
    fn negative_integers_do_not_wrap_to_huge_values() {
        // A negative block count is corruption. Wrapping it to 1.8e19 would
        // then be multiplied into the KV-cache estimate.
        assert_eq!(GgufValue::I32(-1).as_u64(), None);
        assert_eq!(GgufValue::I64(-1).as_u64(), None);
        assert_eq!(GgufValue::I8(-1).as_u64(), None);
    }

    #[test]
    fn as_u32_rejects_values_that_do_not_fit() {
        assert_eq!(GgufValue::U64(u64::from(u32::MAX)).as_u32(), Some(u32::MAX));
        assert_eq!(GgufValue::U64(u64::from(u32::MAX) + 1).as_u32(), None);
    }

    #[test]
    fn floats_coerce_across_widths() {
        assert_eq!(GgufValue::F32(0.5).as_f64(), Some(0.5));
        assert_eq!(GgufValue::F64(0.5).as_f64(), Some(0.5));
        assert_eq!(GgufValue::U32(1).as_f64(), None);
    }

    #[test]
    fn array_length_arithmetic_is_checked() {
        assert_eq!(fixed_array_byte_len(GgufValueType::U32, 10), Some(40));
        // A corrupt length must not overflow into a small number.
        assert_eq!(fixed_array_byte_len(GgufValueType::U64, u64::MAX), None);
        assert_eq!(fixed_array_byte_len(GgufValueType::String, 10), None);
    }

    #[test]
    fn ensure_fits_rejects_a_count_larger_than_the_file() {
        assert!(ensure_fits("keys", 10, 100, 0).is_ok());
        assert!(ensure_fits("keys", 1_000, 100, 0).is_err());
    }
}
