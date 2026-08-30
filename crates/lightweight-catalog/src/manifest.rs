//! The lightweight models this build is known to run, and where to get them.
//!
//! A shortcut, not a fence. Anything with a direct https link can be added
//! through [`crate::install`], and anything already on disk can be imported.
//! What a pinned entry buys is the one thing a pasted link cannot give: a
//! digest recorded **before** the download, so the bytes are checked against a
//! value that did not travel with them.
//!
//! Why these four. Each is a model this project has actually run or read
//! metadata from, and each is small enough to be a reasonable first download on
//! a machine like the development box:
//!
//! * `smollm2-135m-instruct-q4_k_m` — the M3 and M5 real-engine model.
//! * `lfm2-1.2b-q4_k_m` — M1 and M2; the model whose per-layer
//!   `head_count_kv` array made the uniform KV formula wrong.
//! * `qwen3-1.7b-q4_k_m` — M3.6 and M4; reasoning and tool calls.
//! * `gemma-3-1b-it-q4_k_m` — the declared `attention.key_length` case.
//!
//! The heavier models in `scripts/fetch-real-headers.sh` (Llama-3.2-3B,
//! SmolLM3-3B, Phi-4-mini) are deliberately absent: this list is meant to be
//! safe to pick from without thinking about it. They can still be imported or
//! linked.
//!
//! **Digests are recorded, never invented.** `scripts/record-model-digests.sh`
//! reads each file's LFS object id and size from the HuggingFace tree API and
//! prints the literals below, the same way the engine digests came from the
//! GitHub release API. Whether a listed model *fits* is not recorded here at
//! all — the RAM estimator answers that per machine, at load time.

use serde::{Deserialize, Serialize};

/// One model we know how to fetch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogModel {
    /// Catalog id it is installed under.
    pub id: &'static str,
    /// What to show a person.
    pub name: &'static str,
    /// HuggingFace repo, `owner/name`.
    pub repo: &'static str,
    /// File within the repo.
    pub file: &'static str,
    /// Lowercase hex sha256 — the LFS object id.
    pub sha256: &'static str,
    /// Size in bytes.
    pub size: u64,
    /// Parameter count as advertised, for display before anything is fetched.
    pub parameters: &'static str,
    pub quantization: &'static str,
    /// One line on why someone would pick this one.
    pub summary: &'static str,
}

impl CatalogModel {
    /// Where to download this model from.
    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/main/{}",
            self.repo, self.file
        )
    }

    /// The file name it is stored under, which is the repo's own.
    pub fn file_name(&self) -> &'static str {
        self.file
    }
}

/// Every pinned model.
pub const MODELS: &[CatalogModel] = &[
    CatalogModel {
        id: "smollm2-135m-instruct-q4_k_m",
        name: "SmolLM2 135M Instruct",
        repo: "bartowski/SmolLM2-135M-Instruct-GGUF",
        file: "SmolLM2-135M-Instruct-Q4_K_M.gguf",
        sha256: "2e8040ceae7815abe0dcb3540b9995eaa1fa0d2ca9e797d0a635ae4433c68c2d",
        size: 105_454_432,
        parameters: "135M",
        quantization: "Q4_K_M",
        summary: "Smallest useful model here. Loads in seconds and answers on any CPU.",
    },
    CatalogModel {
        id: "lfm2-1.2b-q4_k_m",
        name: "LFM2 1.2B",
        repo: "LiquidAI/LFM2-1.2B-GGUF",
        file: "LFM2-1.2B-Q4_K_M.gguf",
        sha256: "55175400e3f509a9616227afeffd58d87e80b9f628a5d3d54ada884d85221fed",
        size: 730_893_248,
        parameters: "1.2B",
        quantization: "Q4_K_M",
        summary: "A hybrid attention model: only some layers hold a KV cache, so it is cheap for its size.",
    },
    CatalogModel {
        id: "qwen3-1.7b-q4_k_m",
        name: "Qwen3 1.7B",
        repo: "bartowski/Qwen_Qwen3-1.7B-GGUF",
        file: "Qwen_Qwen3-1.7B-Q4_K_M.gguf",
        sha256: "72c5c3cb38fa32d5256e2fe30d03e7a64c6c79e668ad84057e3bd66e250b24fb",
        size: 1_282_439_584,
        parameters: "1.7B",
        quantization: "Q4_K_M",
        summary: "Reasoning and tool calls. Streams its thinking separately from its answer.",
    },
    CatalogModel {
        id: "gemma-3-1b-it-q4_k_m",
        name: "Gemma 3 1B Instruct",
        repo: "unsloth/gemma-3-1b-it-GGUF",
        file: "gemma-3-1b-it-Q4_K_M.gguf",
        sha256: "8270790f3ab69fdfe860b7b64008d9a19986d8df7e407bb018184caa08798ebd",
        size: 806_058_272,
        parameters: "1B",
        quantization: "Q4_K_M",
        summary: "Small instruction-tuned model with a large declared context.",
    },
];

/// Look a pinned model up by id.
pub fn by_id(id: &str) -> Option<&'static CatalogModel> {
    MODELS.iter().find(|model| model.id == id)
}

/// Whether every entry carries a real recorded digest.
///
/// A `PENDING` placeholder means `scripts/record-model-digests.sh` has not been
/// run since an entry was added. Downloading such an entry is refused rather
/// than silently downgraded to an unverified transfer.
pub fn is_recorded(model: &CatalogModel) -> bool {
    model.size > 0
        && model.sha256.len() == 64
        && model.sha256.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_id_is_unique() {
        // Two entries with one id would make `by_id` silently pick the first.
        let mut ids: Vec<&str> = MODELS.iter().map(|model| model.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count, "duplicate id in the manifest");
    }

    #[test]
    fn every_url_is_https_and_names_its_file() {
        for model in MODELS {
            let url = model.url();
            assert!(url.starts_with("https://"), "{url}");
            assert!(url.ends_with(model.file), "{url}");
        }
    }

    #[test]
    fn every_entry_has_a_recorded_digest_and_size() {
        // A `PENDING` placeholder must never ship. Adding an entry means
        // running scripts/record-model-digests.sh and pasting what it prints;
        // this test is what makes forgetting that a failure rather than a
        // silently unverified download.
        for model in MODELS {
            assert!(
                is_recorded(model),
                "{} has no recorded digest or size - run scripts/record-model-digests.sh",
                model.id
            );
        }
    }

    #[test]
    fn a_placeholder_digest_is_never_treated_as_recorded() {
        // The runtime guard behind the test above: the download path checks
        // `is_recorded` so an unrecorded entry is refused rather than fetched
        // without verification.
        let pending = CatalogModel {
            sha256: "PENDING",
            size: 0,
            ..MODELS[0]
        };
        assert!(!is_recorded(&pending));
        // A digest of the right shape but no size is equally unusable.
        assert!(!is_recorded(&CatalogModel {
            size: 0,
            ..MODELS[0]
        }));
    }

    #[test]
    fn a_model_can_be_found_by_id() {
        assert!(by_id("qwen3-1.7b-q4_k_m").is_some());
        assert!(by_id("gpt-4").is_none());
    }
}
