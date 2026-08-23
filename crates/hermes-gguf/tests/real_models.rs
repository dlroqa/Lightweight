//! Parses the headers of real models from HuggingFace.
//!
//! Fixtures the crate generates itself share the author's reading of the
//! format, so they cannot catch a misreading — only a real file can. These
//! headers are captured with an HTTP range request (`scripts/fetch-real-headers.sh`),
//! so the whole set costs a few megabytes rather than the tens of gigabytes the
//! full models would.
//!
//! The captured headers are not committed. When they are absent every test here
//! skips, so a clean checkout still passes; run the fetch script to enable them.
//!
//! A test that silently passes when its inputs are missing is a trap: CI would
//! stay green while the reader broke. So CI sets [`REQUIRE_ENV`], which turns
//! absence into a failure rather than a skip.

use std::path::PathBuf;

use hermes_gguf::{GgufFile, ModelMetadata};

/// Set this to make missing headers a failure instead of a skip.
const REQUIRE_ENV: &str = "HERMES_REQUIRE_REAL_MODELS";

/// Returns the captured models, or `None` if the suite should skip.
///
/// Panics rather than skipping when [`REQUIRE_ENV`] is set, so a CI run that
/// meant to exercise real models cannot quietly exercise nothing.
fn required_or_skip() -> Option<Vec<(String, PathBuf)>> {
    let models = captured();
    if !models.is_empty() {
        return Some(models);
    }
    assert!(
        std::env::var_os(REQUIRE_ENV).is_none(),
        "{REQUIRE_ENV} is set but no captured headers were found in {}; \
         run scripts/fetch-real-headers.sh",
        headers_dir().display()
    );
    eprintln!("skipping: no captured headers; run scripts/fetch-real-headers.sh");
    None
}

fn headers_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/real-headers")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("/nonexistent"))
}

fn captured() -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(headers_dir()) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "gguf"))
        .filter_map(|path| {
            let name = path.file_stem()?.to_str()?.to_owned();
            Some((name, path))
        })
        .collect();
    out.sort();
    out
}

#[test]
fn parses_every_captured_real_model_header() {
    let Some(models) = required_or_skip() else {
        return;
    };

    for (name, path) in models {
        let file = GgufFile::open_header_only(&path)
            .unwrap_or_else(|err| panic!("{name}: header did not parse: {err}"));
        let metadata = ModelMetadata::from_file(&file)
            .unwrap_or_else(|err| panic!("{name}: metadata did not extract: {err}"));

        eprintln!(
            "{name:10} arch={:<8} params={:<7} quant={:<8} ctx={:<7} layers={:<3} \
             heads={}/{} head_dim={} gqa={} vocab={} template={}",
            metadata.architecture,
            metadata.parameters_label().unwrap_or_else(|| "?".into()),
            metadata.quantization_label(),
            metadata
                .context_length
                .map_or_else(|| "?".into(), |c| c.to_string()),
            metadata
                .block_count
                .map_or_else(|| "?".into(), |c| c.to_string()),
            metadata.head_count.unwrap_or(0),
            metadata.kv_heads_for_layer(0).unwrap_or(0),
            metadata.head_dim_k().unwrap_or(0),
            metadata
                .gqa_ratio()
                .map_or_else(|| "?".into(), |r| r.to_string()),
            metadata.vocab_size.unwrap_or(0),
            metadata.tokenizer.has_chat_template,
        );

        // Facts every real model must yield, or the reader is wrong.
        assert!(!metadata.architecture.is_empty(), "{name}: no architecture");
        assert!(
            metadata.supported,
            "{name}: architecture {:?} is not in the engine's supported list",
            metadata.architecture
        );
        assert!(metadata.block_count.is_some(), "{name}: no layer count");
        assert!(
            metadata.context_length.is_some(),
            "{name}: no context length"
        );
        assert!(metadata.head_count.is_some(), "{name}: no head count");
        assert!(metadata.head_count_kv.is_some(), "{name}: no KV head count");
        assert!(
            metadata.head_dim_k().is_some(),
            "{name}: head dimension not derivable"
        );
        assert!(
            metadata.vocab_size.unwrap_or(0) > 1000,
            "{name}: implausible vocabulary size"
        );
        assert!(
            metadata.missing.is_empty(),
            "{name}: missing keys {:?}",
            metadata.missing
        );
    }
}

#[test]
fn real_headers_are_summarized_not_materialized() {
    // A tokenizer vocabulary is the largest thing in these files. Parsing must
    // record its length from the array header, not read 150,000 strings.
    for (name, path) in captured() {
        let Ok(file) = GgufFile::open_header_only(&path) else {
            continue;
        };
        if let Some(summary) = file.get_array("tokenizer.ggml.tokens") {
            assert!(
                summary.len > 1000,
                "{name}: vocabulary summary looks wrong: {summary:?}"
            );
            assert!(
                summary.byte_len > 0,
                "{name}: vocabulary has no byte length"
            );
        }
    }
}

#[test]
fn a_range_fetched_header_is_rejected_by_the_normal_open_path() {
    // These captures are deliberately incomplete files. The load path must
    // refuse them, because loading one would fail inside the engine instead.
    for (name, path) in captured() {
        assert!(
            GgufFile::open(&path).is_err(),
            "{name}: an incomplete file was accepted by the checked open path"
        );
    }
}
