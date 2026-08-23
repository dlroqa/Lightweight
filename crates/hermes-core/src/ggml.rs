//! The ggml tensor type table.
//!
//! Every byte figure the RAM estimator produces traces back to these two
//! numbers per type, so they are not written from memory. They were read out of
//! the pinned engine binary itself by calling `ggml_blck_size`, `ggml_type_size`
//! and `ggml_type_name` through `dlopen` on `libggml-base.so`, and this file is
//! generated from that output. The pinned build is recorded in
//! [`SOURCE_BUILD`]; regenerate when that pin moves.
//!
//! Quantized types store `block_size` elements in `type_size` bytes, so a
//! tensor's footprint is `ceil(elements / block_size) * type_size` — see
//! [`GgmlType::bytes_for_elements`]. Note that the ratio is not a round number
//! of bits: `q4_K` is 144 bytes per 256 elements, which is 4.5 bits per weight
//! once its scales and mins are counted, not 4.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The llama.cpp build these figures were read from.
pub const SOURCE_BUILD: &str = "b10590";

/// A ggml tensor element type.
///
/// Variant names match ggml's own spelling exactly, because the same strings
/// appear in GGUF metadata, in the UI, and in engine flags such as
/// `--cache-type-k q8_0`. Renaming them to Rust casing would mean translating
/// in both directions at every boundary.
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GgmlType {
    /// `f32`: 1 element in 4 bytes.
    F32,
    /// `f16`: 1 element in 2 bytes.
    F16,
    /// `q4_0`: 32 elements in 18 bytes.
    Q4_0,
    /// `q4_1`: 32 elements in 20 bytes.
    Q4_1,
    /// `q5_0`: 32 elements in 22 bytes.
    Q5_0,
    /// `q5_1`: 32 elements in 24 bytes.
    Q5_1,
    /// `q8_0`: 32 elements in 34 bytes.
    Q8_0,
    /// `q8_1`: 32 elements in 36 bytes.
    Q8_1,
    /// `q2_K`: 256 elements in 84 bytes.
    Q2_K,
    /// `q3_K`: 256 elements in 110 bytes.
    Q3_K,
    /// `q4_K`: 256 elements in 144 bytes.
    Q4_K,
    /// `q5_K`: 256 elements in 176 bytes.
    Q5_K,
    /// `q6_K`: 256 elements in 210 bytes.
    Q6_K,
    /// `q8_K`: 256 elements in 292 bytes.
    Q8_K,
    /// `iq2_xxs`: 256 elements in 66 bytes.
    IQ2_XXS,
    /// `iq2_xs`: 256 elements in 74 bytes.
    IQ2_XS,
    /// `iq3_xxs`: 256 elements in 98 bytes.
    IQ3_XXS,
    /// `iq1_s`: 256 elements in 50 bytes.
    IQ1_S,
    /// `iq4_nl`: 32 elements in 18 bytes.
    IQ4_NL,
    /// `iq3_s`: 256 elements in 110 bytes.
    IQ3_S,
    /// `iq2_s`: 256 elements in 82 bytes.
    IQ2_S,
    /// `iq4_xs`: 256 elements in 136 bytes.
    IQ4_XS,
    /// `i8`: 1 element in 1 byte.
    I8,
    /// `i16`: 1 element in 2 bytes.
    I16,
    /// `i32`: 1 element in 4 bytes.
    I32,
    /// `i64`: 1 element in 8 bytes.
    I64,
    /// `f64`: 1 element in 8 bytes.
    F64,
    /// `iq1_m`: 256 elements in 56 bytes.
    IQ1_M,
    /// `bf16`: 1 element in 2 bytes.
    BF16,
    /// `tq1_0`: 256 elements in 54 bytes.
    TQ1_0,
    /// `tq2_0`: 256 elements in 66 bytes.
    TQ2_0,
    /// `mxfp4`: 32 elements in 17 bytes.
    MXFP4,
    /// `nvfp4`: 64 elements in 36 bytes.
    NVFP4,
    /// `q1_0`: 128 elements in 18 bytes.
    Q1_0,
    /// `q2_0`: 64 elements in 18 bytes.
    Q2_0,
    /// A type id this build does not recognise.
    ///
    /// Kept rather than rejected so that a model using a newer quantization can
    /// still be inspected: the RAM estimate degrades to "partial" instead of
    /// being silently wrong, and the UI can say which id it did not understand.
    Unknown(u32),
}

impl GgmlType {
    /// Every type this build knows, in ggml id order.
    pub const ALL: &'static [Self] = &[
        Self::F32,
        Self::F16,
        Self::Q4_0,
        Self::Q4_1,
        Self::Q5_0,
        Self::Q5_1,
        Self::Q8_0,
        Self::Q8_1,
        Self::Q2_K,
        Self::Q3_K,
        Self::Q4_K,
        Self::Q5_K,
        Self::Q6_K,
        Self::Q8_K,
        Self::IQ2_XXS,
        Self::IQ2_XS,
        Self::IQ3_XXS,
        Self::IQ1_S,
        Self::IQ4_NL,
        Self::IQ3_S,
        Self::IQ2_S,
        Self::IQ4_XS,
        Self::I8,
        Self::I16,
        Self::I32,
        Self::I64,
        Self::F64,
        Self::IQ1_M,
        Self::BF16,
        Self::TQ1_0,
        Self::TQ2_0,
        Self::MXFP4,
        Self::NVFP4,
        Self::Q1_0,
        Self::Q2_0,
    ];

    /// Map a raw ggml type id from a GGUF tensor header.
    pub const fn from_id(id: u32) -> Self {
        match id {
            0 => Self::F32,
            1 => Self::F16,
            2 => Self::Q4_0,
            3 => Self::Q4_1,
            6 => Self::Q5_0,
            7 => Self::Q5_1,
            8 => Self::Q8_0,
            9 => Self::Q8_1,
            10 => Self::Q2_K,
            11 => Self::Q3_K,
            12 => Self::Q4_K,
            13 => Self::Q5_K,
            14 => Self::Q6_K,
            15 => Self::Q8_K,
            16 => Self::IQ2_XXS,
            17 => Self::IQ2_XS,
            18 => Self::IQ3_XXS,
            19 => Self::IQ1_S,
            20 => Self::IQ4_NL,
            21 => Self::IQ3_S,
            22 => Self::IQ2_S,
            23 => Self::IQ4_XS,
            24 => Self::I8,
            25 => Self::I16,
            26 => Self::I32,
            27 => Self::I64,
            28 => Self::F64,
            29 => Self::IQ1_M,
            30 => Self::BF16,
            34 => Self::TQ1_0,
            35 => Self::TQ2_0,
            39 => Self::MXFP4,
            40 => Self::NVFP4,
            41 => Self::Q1_0,
            42 => Self::Q2_0,
            other => Self::Unknown(other),
        }
    }

    /// The raw ggml type id.
    pub const fn id(self) -> u32 {
        match self {
            Self::F32 => 0,
            Self::F16 => 1,
            Self::Q4_0 => 2,
            Self::Q4_1 => 3,
            Self::Q5_0 => 6,
            Self::Q5_1 => 7,
            Self::Q8_0 => 8,
            Self::Q8_1 => 9,
            Self::Q2_K => 10,
            Self::Q3_K => 11,
            Self::Q4_K => 12,
            Self::Q5_K => 13,
            Self::Q6_K => 14,
            Self::Q8_K => 15,
            Self::IQ2_XXS => 16,
            Self::IQ2_XS => 17,
            Self::IQ3_XXS => 18,
            Self::IQ1_S => 19,
            Self::IQ4_NL => 20,
            Self::IQ3_S => 21,
            Self::IQ2_S => 22,
            Self::IQ4_XS => 23,
            Self::I8 => 24,
            Self::I16 => 25,
            Self::I32 => 26,
            Self::I64 => 27,
            Self::F64 => 28,
            Self::IQ1_M => 29,
            Self::BF16 => 30,
            Self::TQ1_0 => 34,
            Self::TQ2_0 => 35,
            Self::MXFP4 => 39,
            Self::NVFP4 => 40,
            Self::Q1_0 => 41,
            Self::Q2_0 => 42,
            Self::Unknown(id) => id,
        }
    }

    /// ggml's own name for the type, as it appears in metadata and engine flags.
    pub const fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F16 => "f16",
            Self::Q4_0 => "q4_0",
            Self::Q4_1 => "q4_1",
            Self::Q5_0 => "q5_0",
            Self::Q5_1 => "q5_1",
            Self::Q8_0 => "q8_0",
            Self::Q8_1 => "q8_1",
            Self::Q2_K => "q2_K",
            Self::Q3_K => "q3_K",
            Self::Q4_K => "q4_K",
            Self::Q5_K => "q5_K",
            Self::Q6_K => "q6_K",
            Self::Q8_K => "q8_K",
            Self::IQ2_XXS => "iq2_xxs",
            Self::IQ2_XS => "iq2_xs",
            Self::IQ3_XXS => "iq3_xxs",
            Self::IQ1_S => "iq1_s",
            Self::IQ4_NL => "iq4_nl",
            Self::IQ3_S => "iq3_s",
            Self::IQ2_S => "iq2_s",
            Self::IQ4_XS => "iq4_xs",
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::F64 => "f64",
            Self::IQ1_M => "iq1_m",
            Self::BF16 => "bf16",
            Self::TQ1_0 => "tq1_0",
            Self::TQ2_0 => "tq2_0",
            Self::MXFP4 => "mxfp4",
            Self::NVFP4 => "nvfp4",
            Self::Q1_0 => "q1_0",
            Self::Q2_0 => "q2_0",
            Self::Unknown(_) => "unknown",
        }
    }

    /// Elements per block. `None` for an unrecognised type.
    pub const fn block_size(self) -> Option<u64> {
        match self {
            Self::F32 => Some(1),
            Self::F16 => Some(1),
            Self::Q4_0 => Some(32),
            Self::Q4_1 => Some(32),
            Self::Q5_0 => Some(32),
            Self::Q5_1 => Some(32),
            Self::Q8_0 => Some(32),
            Self::Q8_1 => Some(32),
            Self::Q2_K => Some(256),
            Self::Q3_K => Some(256),
            Self::Q4_K => Some(256),
            Self::Q5_K => Some(256),
            Self::Q6_K => Some(256),
            Self::Q8_K => Some(256),
            Self::IQ2_XXS => Some(256),
            Self::IQ2_XS => Some(256),
            Self::IQ3_XXS => Some(256),
            Self::IQ1_S => Some(256),
            Self::IQ4_NL => Some(32),
            Self::IQ3_S => Some(256),
            Self::IQ2_S => Some(256),
            Self::IQ4_XS => Some(256),
            Self::I8 => Some(1),
            Self::I16 => Some(1),
            Self::I32 => Some(1),
            Self::I64 => Some(1),
            Self::F64 => Some(1),
            Self::IQ1_M => Some(256),
            Self::BF16 => Some(1),
            Self::TQ1_0 => Some(256),
            Self::TQ2_0 => Some(256),
            Self::MXFP4 => Some(32),
            Self::NVFP4 => Some(64),
            Self::Q1_0 => Some(128),
            Self::Q2_0 => Some(64),
            Self::Unknown(_) => None,
        }
    }

    /// Bytes per block. `None` for an unrecognised type.
    pub const fn type_size(self) -> Option<u64> {
        match self {
            Self::F32 => Some(4),
            Self::F16 => Some(2),
            Self::Q4_0 => Some(18),
            Self::Q4_1 => Some(20),
            Self::Q5_0 => Some(22),
            Self::Q5_1 => Some(24),
            Self::Q8_0 => Some(34),
            Self::Q8_1 => Some(36),
            Self::Q2_K => Some(84),
            Self::Q3_K => Some(110),
            Self::Q4_K => Some(144),
            Self::Q5_K => Some(176),
            Self::Q6_K => Some(210),
            Self::Q8_K => Some(292),
            Self::IQ2_XXS => Some(66),
            Self::IQ2_XS => Some(74),
            Self::IQ3_XXS => Some(98),
            Self::IQ1_S => Some(50),
            Self::IQ4_NL => Some(18),
            Self::IQ3_S => Some(110),
            Self::IQ2_S => Some(82),
            Self::IQ4_XS => Some(136),
            Self::I8 => Some(1),
            Self::I16 => Some(2),
            Self::I32 => Some(4),
            Self::I64 => Some(8),
            Self::F64 => Some(8),
            Self::IQ1_M => Some(56),
            Self::BF16 => Some(2),
            Self::TQ1_0 => Some(54),
            Self::TQ2_0 => Some(66),
            Self::MXFP4 => Some(17),
            Self::NVFP4 => Some(36),
            Self::Q1_0 => Some(18),
            Self::Q2_0 => Some(18),
            Self::Unknown(_) => None,
        }
    }
}

impl GgmlType {
    /// Exact bytes needed to store `elements` values of this type.
    ///
    /// Quantization works on whole blocks, so a tensor whose element count is
    /// not a multiple of `block_size` still pays for the final partial block —
    /// hence `div_ceil` rather than a plain division. Returns `None` for an
    /// unrecognised type, which is what propagates a "partial" confidence up to
    /// the RAM estimate rather than letting a zero slip into a sum.
    pub const fn bytes_for_elements(self, elements: u64) -> Option<u64> {
        let (Some(block_size), Some(type_size)) = (self.block_size(), self.type_size()) else {
            return None;
        };
        // `block_size` is never 0 for a known type: the generator skips the
        // removed slots, which are exactly the ones ggml reports as 0.
        Some(elements.div_ceil(block_size) * type_size)
    }

    /// Whether this is a block-quantized type rather than a plain scalar.
    pub const fn is_quantized(self) -> bool {
        match self.block_size() {
            Some(block_size) => block_size > 1,
            None => false,
        }
    }

    /// Average bits per element, including the block's scales and mins.
    ///
    /// This is the honest figure and it is not the number in the type's name:
    /// `q4_K` is 144 bytes per 256 elements, so 4.5 bits per weight, not 4.
    /// The UI should show this rather than parsing the name.
    pub fn bits_per_element(self) -> Option<f64> {
        let block_size = self.block_size()?;
        let type_size = self.type_size()?;
        Some((type_size * 8) as f64 / block_size as f64)
    }

    /// Whether this build understands the type well enough to size it.
    pub const fn is_known(self) -> bool {
        !matches!(self, Self::Unknown(_))
    }

    /// Parse a ggml type name such as `q4_K`. Case-insensitive.
    ///
    /// Deliberately returns `None` for a name this build does not know rather
    /// than an [`GgmlType::Unknown`]: names come from configuration, where a
    /// typo like `q8_o` must be reported, not silently accepted.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|candidate| candidate.name().eq_ignore_ascii_case(name))
    }
}

impl fmt::Display for GgmlType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(id) => write!(f, "unknown({id})"),
            known => f.write_str(known.name()),
        }
    }
}

impl std::str::FromStr for GgmlType {
    type Err = UnknownGgmlTypeName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_name(s).ok_or_else(|| UnknownGgmlTypeName(s.to_owned()))
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown ggml type name {0:?}")]
pub struct UnknownGgmlTypeName(pub String);

/// Serializes as the ggml name, so API payloads and settings files read as
/// `"q4_K"` rather than as an opaque integer.
impl Serialize for GgmlType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for GgmlType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::from_name(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown ggml type {name:?}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spot-checks against the figures read out of `libggml-base.so` at build
    /// b10590. If a future regeneration changes one of these, that is a real
    /// change in the engine's storage layout and every RAM estimate moves with
    /// it — so it should fail loudly here rather than drift silently.
    #[test]
    fn table_matches_the_engine_binary() {
        assert_eq!(GgmlType::F32.block_size(), Some(1));
        assert_eq!(GgmlType::F32.type_size(), Some(4));
        assert_eq!(GgmlType::F16.type_size(), Some(2));
        assert_eq!(GgmlType::BF16.type_size(), Some(2));

        // The K-quants that matter for the models in spec section 5.
        assert_eq!(GgmlType::Q4_K.block_size(), Some(256));
        assert_eq!(GgmlType::Q4_K.type_size(), Some(144));
        assert_eq!(GgmlType::Q5_K.type_size(), Some(176));
        assert_eq!(GgmlType::Q6_K.type_size(), Some(210));
        assert_eq!(GgmlType::Q8_0.block_size(), Some(32));
        assert_eq!(GgmlType::Q8_0.type_size(), Some(34));
    }

    #[test]
    fn kv_cache_element_costs_are_what_the_estimator_assumes() {
        // The KV cache formula multiplies an element count by bytes-per-element.
        // f16 is the default and is exactly 2 bytes; the quantized options are
        // NOT whole bytes, and rounding them would misstate a multi-gigabyte
        // cache by hundreds of megabytes.
        assert_eq!(GgmlType::F16.bits_per_element(), Some(16.0));
        assert_eq!(GgmlType::Q8_0.bits_per_element(), Some(34.0 * 8.0 / 32.0));
        assert_eq!(GgmlType::Q4_0.bits_per_element(), Some(18.0 * 8.0 / 32.0));

        // 34/32 = 1.0625 bytes per element, not 1.
        let per_element = GgmlType::Q8_0.bits_per_element().expect("known type") / 8.0;
        assert!((per_element - 1.0625).abs() < f64::EPSILON);
    }

    #[test]
    fn quantized_names_understate_the_real_bit_width() {
        // q4_K is 4.5 bits per weight once scales and mins are counted. A UI
        // that parses "4" out of the name would under-report every model.
        assert_eq!(GgmlType::Q4_K.bits_per_element(), Some(4.5));
    }

    #[test]
    fn partial_blocks_are_paid_for_in_full() {
        // 257 elements of q4_K is two blocks, not 1.004 blocks.
        assert_eq!(GgmlType::Q4_K.bytes_for_elements(256), Some(144));
        assert_eq!(GgmlType::Q4_K.bytes_for_elements(257), Some(288));
        assert_eq!(GgmlType::Q4_K.bytes_for_elements(1), Some(144));
        assert_eq!(GgmlType::Q4_K.bytes_for_elements(0), Some(0));
    }

    #[test]
    fn scalar_types_are_a_flat_multiple() {
        assert_eq!(GgmlType::F32.bytes_for_elements(1000), Some(4000));
        assert_eq!(GgmlType::F16.bytes_for_elements(1000), Some(2000));
    }

    #[test]
    fn unknown_types_size_to_none_rather_than_zero() {
        // A zero here would silently shrink a model's estimated weight bytes
        // and could turn an INSUFFICIENT verdict into SAFE.
        let unknown = GgmlType::from_id(9999);
        assert!(!unknown.is_known());
        assert_eq!(unknown.bytes_for_elements(1_000_000), None);
        assert_eq!(unknown.block_size(), None);
    }

    #[test]
    fn removed_type_ids_are_treated_as_unknown() {
        // ggml reports ids 4, 5, 31, 32, 33, 36, 37 and 38 as removed, with a
        // block size of 0. Sizing them by that would divide by zero.
        for removed in [4, 5, 31, 32, 33, 36, 37, 38] {
            let ty = GgmlType::from_id(removed);
            assert!(!ty.is_known(), "id {removed} should be unknown");
            assert_eq!(ty.bytes_for_elements(256), None);
        }
    }

    #[test]
    fn every_known_type_can_size_itself() {
        for &ty in GgmlType::ALL {
            assert!(ty.block_size().is_some(), "{ty} has no block size");
            assert!(ty.type_size().is_some(), "{ty} has no type size");
            assert!(
                ty.bytes_for_elements(256).is_some_and(|bytes| bytes > 0),
                "{ty} sized 256 elements as zero"
            );
        }
    }

    #[test]
    fn ids_round_trip() {
        for &ty in GgmlType::ALL {
            assert_eq!(GgmlType::from_id(ty.id()), ty);
        }
    }

    #[test]
    fn names_round_trip_and_ignore_case() {
        for &ty in GgmlType::ALL {
            assert_eq!(GgmlType::from_name(ty.name()), Some(ty));
            assert_eq!(GgmlType::from_name(&ty.name().to_uppercase()), Some(ty));
        }
    }

    #[test]
    fn a_misspelled_type_name_is_rejected_not_absorbed() {
        // `--cache-type-k q8_o` must be an error, not a silent Unknown that
        // later makes the KV estimate unsizeable.
        assert_eq!(GgmlType::from_name("q8_o"), None);
        assert!("q8_o".parse::<GgmlType>().is_err());
        assert_eq!("q8_0".parse::<GgmlType>().ok(), Some(GgmlType::Q8_0));
    }

    #[test]
    fn scalars_are_not_reported_as_quantized() {
        assert!(!GgmlType::F32.is_quantized());
        assert!(!GgmlType::F16.is_quantized());
        assert!(!GgmlType::BF16.is_quantized());
        assert!(GgmlType::Q4_K.is_quantized());
        assert!(GgmlType::Q8_0.is_quantized());
    }

    #[test]
    fn serializes_as_its_ggml_name() {
        let json = serde_json::to_string(&GgmlType::Q4_K).expect("serialize");
        assert_eq!(json, "\"q4_K\"");
        let back: GgmlType = serde_json::from_str("\"q4_K\"").expect("deserialize");
        assert_eq!(back, GgmlType::Q4_K);
    }

    #[test]
    fn deserializing_an_unknown_name_fails_loudly() {
        assert!(serde_json::from_str::<GgmlType>("\"q9_Z\"").is_err());
    }
}
