//! Which model architectures the pinned engine can actually run.
//!
//! Spec section 6 is explicit that the application must never hard-code one
//! architecture, and that support must be decided from model metadata. This
//! table is the single exception, and it is deliberately *data*, not code: it
//! is generated from `LLM_ARCH_NAMES` in `src/llama-arch.cpp` at the pinned
//! build, so it states what the engine we ship genuinely supports rather than
//! what anyone believed it supported.
//!
//! Nothing else in this crate branches on the architecture string.

/// The llama.cpp build this list was generated from.
pub const SOURCE_BUILD: &str = "b10590";

/// Every architecture the pinned engine recognises (147 of them),
/// sorted so the unsupported-model error lists them predictably.
pub const SUPPORTED: &[&str] = &[
    "afmoe",
    "apertus",
    "arcee",
    "arctic",
    "arwkv7",
    "baichuan",
    "bailingmoe",
    "bailingmoe2",
    "bailingmoe3",
    "bert",
    "bitnet",
    "bloom",
    "chameleon",
    "chatglm",
    "clip",
    "codeshell",
    "cogvlm",
    "cohere2",
    "cohere2moe",
    "command-r",
    "dbrx",
    "deci",
    "deepseek",
    "deepseek2",
    "deepseek2-ocr",
    "deepseek32",
    "deepseek4",
    "dflash",
    "dots1",
    "dots3note",
    "dream",
    "eagle3",
    "ernie4_5",
    "ernie4_5-moe",
    "eurobert",
    "exaone",
    "exaone-moe",
    "exaone4",
    "falcon",
    "falcon-h1",
    "gemma",
    "gemma-embedding",
    "gemma2",
    "gemma3",
    "gemma3n",
    "gemma4",
    "gemma4-assistant",
    "glm-dsa",
    "glm4",
    "glm4moe",
    "gpt-oss",
    "gpt2",
    "gptj",
    "gptneox",
    "granite",
    "granite_swa",
    "granitehybrid",
    "granitemoe",
    "graniteswitch",
    "grok",
    "grovemoe",
    "hunyuan-dense",
    "hunyuan-moe",
    "hunyuan_vl",
    "hy_v3",
    "internlm2",
    "jais",
    "jais2",
    "jamba",
    "jina-bert-v2",
    "jina-bert-v3",
    "kimi-k3",
    "kimi-linear",
    "laguna",
    "lfm2",
    "lfm2moe",
    "llada",
    "llada-moe",
    "llama",
    "llama-embed",
    "llama4",
    "maincoder",
    "mamba",
    "mamba2",
    "mellum",
    "mimo2",
    "minicpm",
    "minicpm3",
    "minimax-01",
    "minimax-m2",
    "minimax-m3",
    "mistral3",
    "mistral4",
    "modern-bert",
    "mpt",
    "muse-glimmer",
    "nanbeige",
    "nemotron",
    "nemotron_h",
    "nemotron_h_moe",
    "neo-bert",
    "nomic-bert",
    "nomic-bert-moe",
    "olmo",
    "olmo2",
    "olmoe",
    "openelm",
    "orion",
    "paddleocr",
    "pangu-embedded",
    "phi2",
    "phi3",
    "phimoe",
    "plamo",
    "plamo2",
    "plamo3",
    "plm",
    "pockettts",
    "qwen",
    "qwen2",
    "qwen2moe",
    "qwen2vl",
    "qwen3",
    "qwen35",
    "qwen35moe",
    "qwen3moe",
    "qwen3next",
    "qwen3tts",
    "qwen3vl",
    "qwen3vlmoe",
    "refact",
    "rnd1",
    "rwkv6",
    "rwkv6qwen2",
    "rwkv7",
    "seed_oss",
    "smallthinker",
    "smollm3",
    "stablelm",
    "starcoder",
    "starcoder2",
    "step35",
    "t5",
    "t5encoder",
    "talkie",
    "wavtokenizer-dec",
    "xverse",
];

/// Whether the pinned engine supports `architecture`.
///
/// Comparison is exact: GGUF writes `general.architecture` as one of these
/// tokens, and a near-miss such as `Llama` is a malformed file rather than a
/// spelling we should quietly accept.
pub fn is_supported(architecture: &str) -> bool {
    SUPPORTED.binary_search(&architecture).is_ok()
}

/// Architectures whose names are closest to `architecture`, for an error
/// message that helps rather than just refusing.
///
/// Deliberately simple: a shared prefix of three or more characters. That
/// catches the realistic confusions (`gemma` for `gemma3`, `qwen` for `qwen3`)
/// without pretending to be a spell checker.
pub fn nearest(architecture: &str, limit: usize) -> Vec<&'static str> {
    let needle = architecture.to_ascii_lowercase();
    let mut scored: Vec<(usize, &'static str)> = SUPPORTED
        .iter()
        .filter_map(|&candidate| {
            let shared = candidate
                .bytes()
                .zip(needle.bytes())
                .take_while(|(a, b)| a == b)
                .count();
            (shared >= 3).then_some((shared, candidate))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, name)| name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_sorted_because_lookup_is_a_binary_search() {
        let mut sorted = SUPPORTED.to_vec();
        sorted.sort_unstable();
        assert_eq!(SUPPORTED, sorted.as_slice());
    }

    #[test]
    fn every_architecture_from_spec_section_five_is_supported() {
        // LFM2.5, Phi-4-mini, Llama 3.2, Gemma 3, Qwen3 and SmolLM3 are named
        // as required models. Phi-4-mini reports itself as `phi3`.
        for architecture in ["lfm2", "phi3", "llama", "gemma3", "qwen3", "smollm3"] {
            assert!(
                is_supported(architecture),
                "{architecture} is not supported"
            );
        }
    }

    #[test]
    fn an_unknown_architecture_is_not_supported() {
        assert!(!is_supported("definitely-not-a-model"));
        assert!(!is_supported(""));
    }

    #[test]
    fn matching_is_exact_not_case_insensitive() {
        // GGUF writes the canonical lowercase token; anything else is a
        // malformed file, and silently accepting it would hide that.
        assert!(is_supported("llama"));
        assert!(!is_supported("Llama"));
    }

    #[test]
    fn suggests_close_names_for_a_near_miss() {
        let suggestions = nearest("gemma", 5);
        assert!(
            suggestions.contains(&"gemma3"),
            "expected gemma3 in {suggestions:?}"
        );
    }

    #[test]
    fn suggests_nothing_for_a_wholly_unrelated_name() {
        assert!(nearest("zzzzzzz", 5).is_empty());
    }
}
