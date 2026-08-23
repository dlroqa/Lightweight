//! Architecture-driven metadata extraction.
//!
//! Spec section 6 requires that the application "never hard-code one model
//! architecture" and that compatibility is decided from metadata. GGUF makes
//! that practical: global keys are `general.*`, and per-model keys are
//! `{general.architecture}.{group}.{name}`. So this module reads
//! `general.architecture` first and then *interpolates* it into every other
//! lookup. A new architecture is readable the day it ships, with no code change
//! here.
//!
//! Key names are transcribed from `src/llama-arch.cpp` at build b10590.
//!
//! Two rules that matter more than they look:
//!
//! * **A missing key is `None`, never a default.** Substituting a plausible
//!   value would let the RAM estimator produce a confident number from data it
//!   does not have. Anything absent is recorded in [`ModelMetadata::missing`]
//!   so the estimate can be reported as partial and the UI can say why.
//! * **Head counts may be per-layer.** Most architectures write
//!   `attention.head_count_kv` as one scalar, but hybrid ones write an array
//!   with a zero for layers that have no attention at all. Assuming uniformity
//!   would overstate the KV cache for those models by a wide margin, so both
//!   shapes are handled here rather than at each call site.

use std::collections::BTreeMap;

use hermes_core::GgmlType;
use serde::{Deserialize, Serialize};

use crate::architecture;
use crate::error::GgufError;
use crate::reader::GgufFile;

/// How many tensors of a given type a model contains, and their footprint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuantStat {
    pub tensors: u64,
    pub elements: u64,
    /// `None` when the type is one this build cannot size.
    pub bytes: Option<u64>,
}

/// The mix of tensor types actually present.
///
/// This is the authoritative description of a model's quantization. The
/// `general.file_type` label is a summary written by whoever quantized the
/// file, and it is routinely misleading: a "Q4_K_M" model contains Q6_K and F32
/// tensors as well as Q4_K ones.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QuantMix {
    /// The type holding the largest share of parameters.
    pub dominant: Option<GgmlType>,
    pub by_type: BTreeMap<GgmlType, QuantStat>,
}

impl QuantMix {
    fn from_file(file: &GgufFile) -> Self {
        let mut by_type: BTreeMap<GgmlType, QuantStat> = BTreeMap::new();
        for tensor in file.tensors() {
            // Seeded with `Some(0)`, not `None`: the running total starts at
            // zero bytes, and `None` is reserved for "a tensor of this type
            // could not be sized". Starting from the derived `Default` would
            // make the first tensor collapse every total to `None`.
            let entry = by_type.entry(tensor.ggml_type).or_insert(QuantStat {
                tensors: 0,
                elements: 0,
                bytes: Some(0),
            });
            entry.tensors = entry.tensors.saturating_add(1);
            let elements = tensor.elements().unwrap_or(0);
            entry.elements = entry.elements.saturating_add(elements);
            entry.bytes = match (entry.bytes, tensor.byte_size()) {
                (Some(running), Some(size)) => Some(running.saturating_add(size)),
                // Once one tensor of this type is unsizeable the running total
                // for the type is meaningless, so drop it rather than report a
                // partial sum as if it were complete.
                _ => None,
            };
        }

        let dominant = by_type
            .iter()
            .max_by_key(|(_, stat)| stat.elements)
            .map(|(ty, _)| *ty);

        Self { dominant, by_type }
    }
}

/// Tokenizer facts, read without materializing the vocabulary.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenizerMeta {
    /// `tokenizer.ggml.model`: `llama`, `gpt2`, `bert`, ...
    pub model: Option<String>,
    /// `tokenizer.ggml.pre`: the pre-tokenizer variant.
    pub pre: Option<String>,
    pub bos_token_id: Option<u32>,
    pub eos_token_id: Option<u32>,
    pub eot_token_id: Option<u32>,
    pub padding_token_id: Option<u32>,
    pub add_bos_token: Option<bool>,
    pub add_eos_token: Option<bool>,
    /// Number of tokens, read from the array header rather than by counting.
    pub token_count: Option<u64>,
    /// Whether the file carries a chat template.
    ///
    /// Load-bearing: without one, the engine cannot format a conversation, so
    /// the model is completion-only and cannot serve `/v1/chat/completions`
    /// faithfully. The UI needs to say so before the user loads it.
    pub has_chat_template: bool,
    /// Length of the template in bytes. The template itself is not held here —
    /// it runs to tens of kilobytes and only the engine needs it.
    pub chat_template_len: Option<u64>,
}

/// Everything we can learn about a model without loading its weights.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// `general.architecture`, verbatim.
    pub architecture: String,
    /// Whether the pinned engine can run this architecture.
    pub supported: bool,
    pub name: Option<String>,
    /// `{arch}.context_length`: the largest context the model was trained for.
    pub context_length: Option<u64>,
    /// `{arch}.block_count`: number of layers.
    pub block_count: Option<u64>,
    /// `{arch}.embedding_length`.
    pub embedding_length: Option<u64>,
    pub feed_forward_length: Option<u64>,
    /// `{arch}.attention.head_count`: query heads.
    pub head_count: Option<u64>,
    /// `{arch}.attention.head_count_kv`, always as a per-layer vector.
    ///
    /// A scalar in the file becomes a one-element vector; use
    /// [`ModelMetadata::kv_heads_for_layer`] rather than indexing directly.
    pub head_count_kv: Option<Vec<u64>>,
    /// `{arch}.attention.key_length`: overrides the derived head dimension.
    pub key_length: Option<u64>,
    pub value_length: Option<u64>,
    /// `{arch}.attention.sliding_window`: the window for architectures that use
    /// windowed attention, which bounds the KV cache below `n_ctx`.
    pub sliding_window: Option<u64>,
    pub rope_freq_base: Option<f64>,
    pub vocab_size: Option<u64>,
    pub tokenizer: TokenizerMeta,
    /// `general.file_type`, a `llama_ftype` value. A label only.
    pub file_type: Option<u32>,
    pub quantization: QuantMix,
    pub tensor_count: u64,
    pub param_count: Option<u64>,
    /// Exact bytes the weights occupy. `None` if any tensor type is unknown.
    pub weight_bytes: Option<u64>,
    pub gguf_version: u32,
    pub alignment: u64,
    /// Keys that were looked for and not found, in the order looked for.
    ///
    /// Anything here means a downstream estimate is incomplete rather than
    /// wrong, and the UI should say which facts are missing.
    pub missing: Vec<String>,
}

impl ModelMetadata {
    /// Extract metadata from a parsed header.
    pub fn from_file(file: &GgufFile) -> Result<Self, GgufError> {
        let architecture = file
            .get_str("general.architecture")
            .unwrap_or_default()
            .to_owned();

        let mut missing = Vec::new();
        let key = |group: &str| format!("{architecture}.{group}");

        let mut want_u64 = |file: &GgufFile, k: String| -> Option<u64> {
            let value = file.get_u64(&k);
            if value.is_none() {
                missing.push(k);
            }
            value
        };

        let context_length = want_u64(file, key("context_length"));
        let block_count = want_u64(file, key("block_count"));
        let embedding_length = want_u64(file, key("embedding_length"));
        let feed_forward_length = want_u64(file, key("feed_forward_length"));
        let head_count = want_u64(file, key("attention.head_count"));

        // Optional refinements: absent for most models and not worth reporting
        // as missing, because their absence has a well-defined meaning.
        let key_length = file.get_u64(&key("attention.key_length"));
        let value_length = file.get_u64(&key("attention.value_length"));
        let sliding_window = file.get_u64(&key("attention.sliding_window"));
        let rope_freq_base = file.get_f64(&key("rope.freq_base"));

        // Scalar or per-layer array; `read_u64_array` normalizes both.
        let kv_key = key("attention.head_count_kv");
        let head_count_kv = file.read_u64_array(&kv_key)?;
        if head_count_kv.is_none() {
            missing.push(kv_key);
        }

        let token_count = file
            .get_array("tokenizer.ggml.tokens")
            .map(|summary| summary.len);
        let vocab_size = token_count.or_else(|| file.get_u64(&key("vocab_size")));

        let chat_template = file.get_str("tokenizer.chat_template");
        let tokenizer = TokenizerMeta {
            model: file.get_str("tokenizer.ggml.model").map(str::to_owned),
            pre: file.get_str("tokenizer.ggml.pre").map(str::to_owned),
            bos_token_id: file.get_u32("tokenizer.ggml.bos_token_id"),
            eos_token_id: file.get_u32("tokenizer.ggml.eos_token_id"),
            eot_token_id: file.get_u32("tokenizer.ggml.eot_token_id"),
            padding_token_id: file.get_u32("tokenizer.ggml.padding_token_id"),
            add_bos_token: file.get_bool("tokenizer.ggml.add_bos_token"),
            add_eos_token: file.get_bool("tokenizer.ggml.add_eos_token"),
            token_count,
            has_chat_template: chat_template.is_some(),
            chat_template_len: chat_template.map(|t| t.len() as u64),
        };

        Ok(Self {
            supported: architecture::is_supported(&architecture),
            architecture,
            name: file.get_str("general.name").map(str::to_owned),
            context_length,
            block_count,
            embedding_length,
            feed_forward_length,
            head_count,
            head_count_kv,
            key_length,
            value_length,
            sliding_window,
            rope_freq_base,
            vocab_size,
            tokenizer,
            file_type: file.get_u32("general.file_type"),
            quantization: QuantMix::from_file(file),
            tensor_count: file.tensors().len() as u64,
            param_count: file.parameter_count(),
            weight_bytes: file.weight_bytes(),
            gguf_version: file.version(),
            alignment: file.alignment(),
            missing,
        })
    }

    /// Dimension of each key head.
    ///
    /// `attention.key_length` when the file states it, otherwise
    /// `embedding_length / head_count`. Returns `None` rather than guessing
    /// when neither is available, or when `head_count` is zero.
    pub fn head_dim_k(&self) -> Option<u64> {
        // `checked_div` rather than a preceding zero check: it makes the
        // division's precondition part of the expression instead of something a
        // later edit could separate it from.
        self.key_length
            .or_else(|| self.embedding_length?.checked_div(self.head_count?))
    }

    /// Dimension of each value head. Usually equal to [`Self::head_dim_k`], but
    /// some architectures state them separately.
    pub fn head_dim_v(&self) -> Option<u64> {
        self.value_length.or_else(|| self.head_dim_k())
    }

    /// KV heads in layer `layer`.
    ///
    /// A file that wrote one scalar means the same count for every layer, so a
    /// one-element vector answers for any index. A per-layer vector is indexed
    /// directly, and a zero means that layer has no attention and so no KV
    /// cache at all.
    pub fn kv_heads_for_layer(&self, layer: u64) -> Option<u64> {
        let heads = self.head_count_kv.as_ref()?;
        match heads.len() {
            0 => None,
            1 => heads.first().copied(),
            _ => heads.get(usize::try_from(layer).ok()?).copied(),
        }
    }

    /// Whether KV head counts vary between layers.
    pub fn has_per_layer_kv_heads(&self) -> bool {
        self.head_count_kv
            .as_ref()
            .is_some_and(|heads| heads.len() > 1)
    }

    /// The grouped-query ratio: query heads per KV head.
    ///
    /// A ratio above 1 is what makes the KV cache dramatically smaller than a
    /// naive estimate, so it is worth surfacing.
    ///
    /// Measured against the first layer that *has* attention. Hybrid models
    /// such as LFM2 begin with convolution layers whose KV head count is zero,
    /// and reporting "unknown" for those would hide a ratio that is perfectly
    /// well defined for every attention layer in the model.
    pub fn gqa_ratio(&self) -> Option<u64> {
        let head_count = self.head_count?;
        // A ratio is only meaningful with a positive count on both sides. Zero
        // query heads would divide out to 0, which reads as a real answer while
        // actually meaning "the geometry is missing".
        if head_count == 0 {
            return None;
        }
        let kv_heads = self
            .head_count_kv
            .as_ref()?
            .iter()
            .copied()
            .find(|&heads| heads > 0)?;
        head_count.checked_div(kv_heads)
    }

    /// Human label for the quantization, preferring the file's own
    /// `general.file_type` and falling back to the dominant tensor type.
    pub fn quantization_label(&self) -> String {
        self.file_type
            .and_then(ftype_name)
            .map(str::to_owned)
            .or_else(|| {
                self.quantization
                    .dominant
                    .map(|ty| ty.name().to_ascii_uppercase())
            })
            .unwrap_or_else(|| "unknown".to_owned())
    }

    /// Parameter count as people quote it: `1.2B`, `340M`.
    pub fn parameters_label(&self) -> Option<String> {
        self.param_count.map(format_parameters)
    }

    /// Whether every fact the RAM estimator needs is present.
    pub fn is_complete_for_estimation(&self) -> bool {
        self.block_count.is_some()
            && self.head_dim_k().is_some()
            && self.head_dim_v().is_some()
            && self.head_count_kv.is_some()
            && self.weight_bytes.is_some()
    }
}

/// Format a parameter count the way model names quote it.
///
/// Split out from [`ModelMetadata::parameters_label`] so it can be tested over
/// the whole range without building a fixture file for each case — a fixture
/// declaring a billion parameters would materialize gigabytes of tensor data.
fn format_parameters(params: u64) -> String {
    if params >= 1_000_000_000 {
        format!("{:.1}B", params as f64 / 1e9)
    } else if params >= 1_000_000 {
        format!("{:.0}M", params as f64 / 1e6)
    } else {
        params.to_string()
    }
}

/// Map a `general.file_type` value to its `llama_ftype` name.
///
/// Transcribed from `enum llama_ftype` in `include/llama.h` at b10590. The
/// commented-out entries there are removed formats and are absent here, so a
/// file claiming one gets no label rather than a wrong one.
fn ftype_name(file_type: u32) -> Option<&'static str> {
    Some(match file_type {
        0 => "F32",
        1 => "F16",
        2 => "Q4_0",
        3 => "Q4_1",
        7 => "Q8_0",
        8 => "Q5_0",
        9 => "Q5_1",
        10 => "Q2_K",
        11 => "Q3_K_S",
        12 => "Q3_K_M",
        13 => "Q3_K_L",
        14 => "Q4_K_S",
        15 => "Q4_K_M",
        16 => "Q5_K_S",
        17 => "Q5_K_M",
        18 => "Q6_K",
        19 => "IQ2_XXS",
        20 => "IQ2_XS",
        21 => "Q2_K_S",
        22 => "IQ3_XS",
        23 => "IQ3_XXS",
        24 => "IQ1_S",
        25 => "IQ4_NL",
        26 => "IQ3_S",
        27 => "IQ3_M",
        28 => "IQ2_S",
        29 => "IQ2_M",
        30 => "IQ4_XS",
        31 => "IQ1_M",
        32 => "BF16",
        36 => "TQ1_0",
        37 => "TQ2_0",
        38 => "MXFP4_MOE",
        39 => "NVFP4",
        40 => "Q1_0",
        41 => "Q2_0",
        1024 => "GUESSED",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{FixtureValue, GgufBuilder, TempDir};

    fn parse(builder: &GgufBuilder) -> (TempDir, ModelMetadata) {
        let dir = TempDir::new("metadata");
        let path = dir.write("model.gguf", &builder.build());
        let file = GgufFile::open(&path).expect("fixture parses");
        let metadata = ModelMetadata::from_file(&file).expect("metadata extracts");
        (dir, metadata)
    }

    #[test]
    fn keys_are_interpolated_from_the_architecture_not_hard_coded() {
        // The same reader must handle every architecture. If any key were
        // hard-coded to "llama.", this would come back empty.
        for architecture in ["llama", "qwen3", "gemma3", "phi3", "lfm2", "smollm3"] {
            let (_dir, metadata) = parse(&GgufBuilder::small_model(architecture));
            assert_eq!(metadata.architecture, architecture);
            assert_eq!(metadata.block_count, Some(2), "{architecture}");
            assert_eq!(metadata.embedding_length, Some(64), "{architecture}");
            assert_eq!(metadata.head_count, Some(8), "{architecture}");
            assert!(
                metadata.missing.is_empty(),
                "{architecture}: {:?}",
                metadata.missing
            );
        }
    }

    #[test]
    fn a_brand_new_architecture_still_reads_its_geometry() {
        // The point of section 6: an architecture nobody has heard of parses
        // fine, and is only reported as unsupported by the engine.
        let (_dir, metadata) = parse(&GgufBuilder::small_model("some-future-arch"));
        assert_eq!(metadata.block_count, Some(2));
        assert_eq!(metadata.head_count, Some(8));
        assert!(!metadata.supported);
    }

    #[test]
    fn known_architectures_are_reported_as_supported() {
        let (_dir, metadata) = parse(&GgufBuilder::small_model("llama"));
        assert!(metadata.supported);
    }

    #[test]
    fn missing_keys_are_recorded_rather_than_defaulted() {
        // A default here would let the RAM estimator produce a confident number
        // from data it does not have.
        let builder = GgufBuilder::new()
            .kv("general.architecture", "llama")
            .tensor("w", &[64], GgmlType::F32);
        let (_dir, metadata) = parse(&builder);

        assert_eq!(metadata.block_count, None);
        assert_eq!(metadata.context_length, None);
        assert!(metadata.missing.contains(&"llama.block_count".to_owned()));
        assert!(
            metadata
                .missing
                .contains(&"llama.context_length".to_owned())
        );
        assert!(!metadata.is_complete_for_estimation());
    }

    #[test]
    fn head_dimension_is_derived_when_not_stated() {
        // embedding_length 64 / head_count 8
        let (_dir, metadata) = parse(&GgufBuilder::small_model("llama"));
        assert_eq!(metadata.head_dim_k(), Some(8));
        assert_eq!(metadata.head_dim_v(), Some(8));
    }

    #[test]
    fn an_explicit_key_length_overrides_the_derived_dimension() {
        // Some architectures state a head dimension that is not
        // embedding_length / head_count. Deriving anyway would misplace every
        // KV byte.
        let builder = GgufBuilder::small_model("llama")
            .kv("llama.attention.key_length", 128u32)
            .kv("llama.attention.value_length", 256u32);
        let (_dir, metadata) = parse(&builder);
        assert_eq!(metadata.head_dim_k(), Some(128));
        assert_eq!(metadata.head_dim_v(), Some(256));
    }

    #[test]
    fn value_length_falls_back_to_the_key_dimension() {
        let builder = GgufBuilder::small_model("llama").kv("llama.attention.key_length", 128u32);
        let (_dir, metadata) = parse(&builder);
        assert_eq!(metadata.head_dim_v(), Some(128));
    }

    #[test]
    fn a_zero_head_count_does_not_divide_by_zero() {
        let builder = GgufBuilder::small_model("llama").kv("llama.attention.head_count", 0u32);
        let (_dir, metadata) = parse(&builder);
        assert_eq!(metadata.head_dim_k(), None);
        assert_eq!(metadata.gqa_ratio(), None);
    }

    #[test]
    fn a_scalar_kv_head_count_answers_for_every_layer() {
        let (_dir, metadata) = parse(&GgufBuilder::small_model("llama"));
        assert!(!metadata.has_per_layer_kv_heads());
        assert_eq!(metadata.kv_heads_for_layer(0), Some(2));
        assert_eq!(metadata.kv_heads_for_layer(99), Some(2));
    }

    #[test]
    fn per_layer_kv_head_counts_are_read_per_layer() {
        // Hybrid architectures write a zero for layers with no attention.
        // Treating the first element as uniform would overstate the KV cache
        // for every one of those layers.
        let builder = GgufBuilder::small_model("lfm2").kv(
            "lfm2.attention.head_count_kv",
            FixtureValue::U32Array(vec![0, 8, 0, 8]),
        );
        let (_dir, metadata) = parse(&builder);

        assert!(metadata.has_per_layer_kv_heads());
        assert_eq!(metadata.kv_heads_for_layer(0), Some(0));
        assert_eq!(metadata.kv_heads_for_layer(1), Some(8));
        assert_eq!(metadata.kv_heads_for_layer(2), Some(0));
        assert_eq!(metadata.kv_heads_for_layer(3), Some(8));
        // Beyond the array is unknown, not "the first value".
        assert_eq!(metadata.kv_heads_for_layer(4), None);
    }

    #[test]
    fn per_type_byte_totals_sum_to_the_model_size() {
        // The headline "model size" and the per-type breakdown are computed
        // separately and must agree, or one of them is lying to the user.
        let dir = TempDir::new("quantsum");
        let path = dir.write("model.gguf", &GgufBuilder::small_model("llama").build());
        let file = GgufFile::open(&path).expect("parse");
        let metadata = ModelMetadata::from_file(&file).expect("metadata");

        let summed: u64 = metadata
            .quantization
            .by_type
            .values()
            .map(|stat| stat.bytes.expect("every known type must be sizeable"))
            .sum();
        assert_eq!(Some(summed), metadata.weight_bytes);
        assert_eq!(summed, file.tensor_data_bytes());
    }

    #[test]
    fn one_unsizeable_tensor_only_spoils_its_own_type() {
        let builder =
            GgufBuilder::small_model("llama").tensor("mystery", &[64], GgmlType::Unknown(9999));
        let (_dir, metadata) = parse(&builder);
        // q4_K tensors are still perfectly sizeable.
        assert!(
            metadata.quantization.by_type[&GgmlType::Q4_K]
                .bytes
                .is_some()
        );
        assert!(
            metadata.quantization.by_type[&GgmlType::Unknown(9999)]
                .bytes
                .is_none()
        );
    }

    #[test]
    fn the_gqa_ratio_skips_layers_that_have_no_attention() {
        // LFM2's real pattern: layer 0 is a convolution block with zero KV
        // heads, but the model's GQA ratio is still 32/8 = 4.
        let builder = GgufBuilder::small_model("lfm2").kv(
            "lfm2.attention.head_count_kv",
            FixtureValue::U32Array(vec![0, 0, 8, 0]),
        );
        let (_dir, metadata) = parse(&builder);
        assert_eq!(metadata.gqa_ratio(), Some(1)); // head_count is 8 in the fixture
    }

    #[test]
    fn reports_the_grouped_query_ratio() {
        // head_count 8, head_count_kv 2 => 4 query heads share each KV head,
        // so the cache is a quarter the size of a multi-head model's.
        let (_dir, metadata) = parse(&GgufBuilder::small_model("llama"));
        assert_eq!(metadata.gqa_ratio(), Some(4));
    }

    #[test]
    fn reads_the_sliding_window_when_present() {
        // Bounds the KV cache below n_ctx for architectures that use it.
        let builder =
            GgufBuilder::small_model("gemma3").kv("gemma3.attention.sliding_window", 1024u32);
        let (_dir, metadata) = parse(&builder);
        assert_eq!(metadata.sliding_window, Some(1024));
    }

    #[test]
    fn the_quantization_mix_comes_from_the_tensors_not_the_label() {
        // A "Q4_K_M" file is a mix. The histogram is what the estimator uses.
        let (_dir, metadata) = parse(&GgufBuilder::small_model("llama"));

        let q4k = metadata.quantization.by_type[&GgmlType::Q4_K];
        assert_eq!(q4k.tensors, 3);
        let f32_stat = metadata.quantization.by_type[&GgmlType::F32];
        assert_eq!(f32_stat.tensors, 1);
        assert_eq!(metadata.quantization.dominant, Some(GgmlType::Q4_K));
    }

    #[test]
    fn quantization_label_prefers_the_files_own_ftype() {
        // 15 is LLAMA_FTYPE_MOSTLY_Q4_K_M. Users recognise "Q4_K_M"; the
        // dominant tensor type alone would say "Q4_K".
        let builder = GgufBuilder::small_model("llama").kv("general.file_type", 15u32);
        let (_dir, metadata) = parse(&builder);
        assert_eq!(metadata.quantization_label(), "Q4_K_M");
    }

    #[test]
    fn quantization_label_falls_back_to_the_dominant_type() {
        let (_dir, metadata) = parse(&GgufBuilder::small_model("llama"));
        assert_eq!(metadata.file_type, None);
        assert_eq!(metadata.quantization_label(), "Q4_K");
    }

    #[test]
    fn an_unrecognised_ftype_does_not_produce_a_wrong_label() {
        // Removed ftypes such as 33 (Q4_0_4_4) must not map to anything.
        let builder = GgufBuilder::small_model("llama").kv("general.file_type", 33u32);
        let (_dir, metadata) = parse(&builder);
        assert_eq!(metadata.quantization_label(), "Q4_K");
    }

    #[test]
    fn reads_tokenizer_facts_without_materializing_the_vocabulary() {
        let (_dir, metadata) = parse(&GgufBuilder::small_model("llama"));
        assert_eq!(metadata.tokenizer.model.as_deref(), Some("llama"));
        assert_eq!(metadata.tokenizer.bos_token_id, Some(1));
        assert_eq!(metadata.tokenizer.eos_token_id, Some(2));
        assert_eq!(metadata.tokenizer.token_count, Some(128));
        assert_eq!(metadata.vocab_size, Some(128));
    }

    #[test]
    fn notes_whether_a_chat_template_is_present() {
        // Without one the model cannot serve /v1/chat/completions faithfully,
        // so the UI has to be able to warn before the user loads it.
        let (_dir, with) = parse(&GgufBuilder::small_model("llama"));
        assert!(with.tokenizer.has_chat_template);

        let without = GgufBuilder::new()
            .kv("general.architecture", "llama")
            .tensor("w", &[8], GgmlType::F32);
        let (_dir, without) = parse(&without);
        assert!(!without.tokenizer.has_chat_template);
    }

    #[test]
    fn formats_the_parameter_count_the_way_people_quote_it() {
        assert_eq!(format_parameters(1_200_000_000), "1.2B");
        assert_eq!(format_parameters(3_800_000_000), "3.8B");
        assert_eq!(format_parameters(340_000_000), "340M");
        assert_eq!(format_parameters(1_000_000), "1M");
        assert_eq!(format_parameters(999_999), "999999");
    }

    #[test]
    fn parameter_label_reflects_the_tensors_actually_declared() {
        let (_dir, metadata) = parse(&GgufBuilder::small_model("llama"));
        // 64*128 + 64*64 + 64*64 + 64 = 16,448 parameters.
        assert_eq!(metadata.param_count, Some(16_448));
        assert_eq!(metadata.parameters_label().as_deref(), Some("16448"));
    }

    #[test]
    fn completeness_reflects_what_the_estimator_actually_needs() {
        let (_dir, complete) = parse(&GgufBuilder::small_model("llama"));
        assert!(complete.is_complete_for_estimation());

        // An unknown tensor type makes the weight total unknowable.
        let builder =
            GgufBuilder::small_model("llama").tensor("mystery", &[64], GgmlType::Unknown(9999));
        let (_dir, incomplete) = parse(&builder);
        assert_eq!(incomplete.weight_bytes, None);
        assert!(!incomplete.is_complete_for_estimation());
    }

    #[test]
    fn a_file_with_no_architecture_key_does_not_panic() {
        let builder = GgufBuilder::new().kv("general.name", "nameless");
        let (_dir, metadata) = parse(&builder);
        assert_eq!(metadata.architecture, "");
        assert!(!metadata.supported);
    }

    #[test]
    fn metadata_serializes_for_the_api_and_the_catalog() {
        let (_dir, metadata) = parse(&GgufBuilder::small_model("llama"));
        let json = serde_json::to_value(&metadata).expect("serialize");
        assert_eq!(json["architecture"], "llama");
        assert_eq!(json["supported"], true);
        // ggml types are map keys, and must serialize as their names.
        assert!(json["quantization"]["by_type"]["q4_K"].is_object());
    }
}
