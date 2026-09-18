//! `hermes models` — the catalog, without a server running.
//!
//! Everything here works against the catalog file directly. That is the point:
//! "what models does this machine have, and will this one fit?" is a question
//! about the machine, not about a process, and answering it must not require
//! starting an engine.

use std::fmt::Write as _;
use std::path::Path;

use lightweight_catalog::install::{AddModel, InstallProgress, Installer};
use lightweight_catalog::{CatalogStore, InstalledModel, manifest};
use lightweight_core::units::Bytes;
use lightweight_system_info::DataPaths;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Open the catalog for the current profile.
pub fn open(paths: &DataPaths) -> Result<CatalogStore, String> {
    CatalogStore::open(paths.catalog_file()).map_err(crate::serve::describe)
}

/// `hermes models list`.
pub fn list(out: &mut String, store: &CatalogStore) {
    if store.is_empty() {
        let _ = writeln!(
            out,
            "No models yet.\n\n  hermes models available          what can be downloaded\n  hermes models add <id>           download one\n  hermes models import <file>      register a .gguf you already have"
        );
        return;
    }

    let _ = writeln!(
        out,
        "{:<32} {:>9}  {:<10} {:<9} INTEGRITY",
        "ID", "SIZE", "ARCH", "CONTEXT"
    );
    for model in store.models() {
        let context = model
            .context_length
            .map_or_else(|| "unknown".to_owned(), |ctx| ctx.to_string());
        let mut flags = String::new();
        if !model.is_present() {
            // Kept, not deleted: the drive may simply not be mounted.
            flags.push_str("  [file missing]");
        }
        if !model.supported {
            flags.push_str("  [architecture not supported by this engine]");
        }
        let _ = writeln!(
            out,
            "{:<32} {:>9}  {:<10} {:<9} {}{}",
            model.id,
            Bytes(model.bytes).to_string(),
            model.architecture,
            context,
            model.integrity.label(),
            flags
        );
    }
}

/// `hermes models available`.
pub fn available(out: &mut String, store: &CatalogStore) {
    let _ = writeln!(out, "Models this build is known to run:\n");
    for model in manifest::MODELS {
        let installed = store.get(model.id).is_some();
        let _ = writeln!(
            out,
            "  {:<32} {:>9}  {} {}",
            model.id,
            Bytes(model.size).to_string(),
            model.parameters,
            if installed { "(installed)" } else { "" }
        );
        let _ = writeln!(out, "  {:<32} {}", "", model.summary);
    }
    let _ = writeln!(
        out,
        "\nAnything else with a direct https link works too:\n  hermes models add --url <link> [--sha256 <digest>]"
    );
}

/// `hermes models import <path>`.
pub async fn import(
    out: &mut String,
    paths: &DataPaths,
    store: &mut CatalogStore,
    path: &Path,
) -> Result<(), String> {
    let installer = installer(paths)?;
    let (tx, reporter) = reporter("hashing");
    let model = installer
        .import(store, path, &tx)
        .await
        .map_err(crate::serve::describe)?;
    drop(tx);
    let _ = reporter.await;

    describe_added(out, &model);
    Ok(())
}

/// `hermes models add <id>` and `hermes models add --url <link>`.
pub async fn add(
    out: &mut String,
    paths: &DataPaths,
    store: &mut CatalogStore,
    request: &AddModel,
) -> Result<(), String> {
    let installer = installer(paths)?;
    let (tx, reporter) = reporter("downloading");

    // Ctrl-C leaves the partial file in place on purpose, so re-running the
    // command resumes rather than starting again.
    let cancel = CancellationToken::new();
    let interrupt = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            interrupt.cancel();
        }
    });

    let model = installer
        .add(store, request, &tx, &cancel)
        .await
        .map_err(crate::serve::describe)?;
    drop(tx);
    let _ = reporter.await;

    describe_added(out, &model);
    Ok(())
}

/// `hermes models remove <id>`.
pub fn remove(
    out: &mut String,
    store: &mut CatalogStore,
    id: &str,
    delete_file: bool,
) -> Result<(), String> {
    let model = store.remove(id).map_err(crate::serve::describe)?;
    store.save().map_err(crate::serve::describe)?;

    let _ = writeln!(out, "removed {} from the catalog", model.id);

    // An imported file belongs to the user and was never copied, so deleting it
    // would be deleting something we do not own. The record answers this, so the
    // CLI and the control API cannot disagree about it.
    let ours = model.is_ours_to_delete();
    if delete_file {
        if ours {
            match std::fs::remove_file(&model.path) {
                Ok(()) => {
                    let _ = writeln!(out, "deleted {}", model.path.display());
                }
                Err(err) => {
                    let _ = writeln!(out, "could not delete {}: {err}", model.path.display());
                }
            }
        } else {
            let _ = writeln!(
                out,
                "left {} where it is: it was imported, not downloaded, so it is not ours to delete",
                model.path.display()
            );
        }
    } else if ours && model.path.is_file() {
        let _ = writeln!(
            out,
            "the file is still at {} — pass --delete to remove it too",
            model.path.display()
        );
    }
    Ok(())
}

fn installer(paths: &DataPaths) -> Result<Installer, String> {
    Installer::new(paths.models_dir(), paths.downloads_dir()).map_err(crate::serve::describe)
}

fn describe_added(out: &mut String, model: &InstalledModel) {
    let _ = writeln!(out, "\n{}  ({})", model.id, model.name);
    let _ = writeln!(out, "  file       {}", model.path.display());
    let _ = writeln!(out, "  size       {}", Bytes(model.bytes));
    let _ = writeln!(out, "  sha256     {}", model.sha256);
    let _ = writeln!(out, "  integrity  {}", model.integrity.label());
    let _ = writeln!(
        out,
        "  model      {} {}{}",
        model.architecture,
        model.quantization.as_deref().unwrap_or(""),
        if model.supported {
            String::new()
        } else {
            "  (this engine cannot run this architecture)".to_owned()
        }
    );
    if let Some(ctx) = model.context_length {
        let _ = writeln!(out, "  context    up to {ctx} tokens");
    }
    let _ = writeln!(
        out,
        "\nWhat it costs to load here:\n  hermes estimate {} --ctx 4096",
        model.path.display()
    );
}

/// Print progress on one line, redrawing only when the whole percent changes.
///
/// The same discipline as the engine download: a 100 MB transfer arrives in
/// thousands of chunks, and a line per chunk buries everything else.
fn reporter(verb: &'static str) -> (mpsc::Sender<InstallProgress>, tokio::task::JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel(64);
    let handle = tokio::spawn(async move {
        let mut last = u64::MAX;
        while let Some(update) = rx.recv().await {
            let (done, total) = match update {
                InstallProgress::Downloading { downloaded, total } => (downloaded, total),
                InstallProgress::Hashing { done, total } => (done, Some(total)),
                InstallProgress::Reading => {
                    print_line("  reading the model header");
                    continue;
                }
                InstallProgress::Resolving | InstallProgress::Done => continue,
            };
            if let Some(percent) = done.saturating_mul(100).checked_div(total.unwrap_or(0))
                && percent != last
            {
                last = percent;
                print_progress(verb, percent, done, total);
            }
        }
    });
    (tx, handle)
}

fn print_progress(verb: &str, percent: u64, done: u64, total: Option<u64>) {
    use std::io::Write as _;
    let of = total.map_or_else(String::new, |total| format!(" of {}", Bytes(total)));
    print!("\r  {verb} {percent:>3}%  {}{of}   ", Bytes(done));
    let _ = std::io::stdout().flush();
}

fn print_line(text: &str) {
    use std::io::Write as _;
    println!("\r{text:<50}");
    let _ = std::io::stdout().flush();
}

/// Turn the CLI's two ways of naming a model into one request.
pub fn add_request(
    id: Option<&str>,
    url: Option<&str>,
    sha256: Option<&str>,
) -> Result<AddModel, String> {
    match (id, url) {
        (Some(id), None) => Ok(AddModel::Pinned { id: id.to_owned() }),
        (None, Some(url)) => Ok(AddModel::Link {
            url: url.to_owned(),
            sha256: sha256.map(str::to_owned),
        }),
        (Some(_), Some(_)) => Err("give either a pinned id or --url, not both".to_owned()),
        (None, None) => Err(
            "name a pinned model or pass --url. `hermes models available` lists the pinned ones."
                .to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_id_and_a_link_are_two_ways_of_asking_not_one() {
        assert!(matches!(
            add_request(Some("qwen3-1.7b-q4_k_m"), None, None),
            Ok(AddModel::Pinned { .. })
        ));
        assert!(matches!(
            add_request(None, Some("https://x/m.gguf"), None),
            Ok(AddModel::Link { sha256: None, .. })
        ));
        // Both, or neither, is a mistake worth naming rather than guessing at.
        assert!(add_request(Some("a"), Some("https://x/m.gguf"), None).is_err());
        assert!(add_request(None, None, None).is_err());
    }

    #[test]
    fn an_empty_catalog_says_what_to_do_next_rather_than_nothing() {
        let mut out = String::new();
        list(&mut out, &CatalogStore::in_memory());
        assert!(out.contains("hermes models available"));
        assert!(out.contains("hermes models import"));
    }

    #[test]
    fn the_pinned_list_offers_the_link_route_as_well() {
        // The pinned models are a shortcut, and a user who cannot find what
        // they want there must be told the other door exists.
        let mut out = String::new();
        available(&mut out, &CatalogStore::in_memory());
        assert!(out.contains("--url"));
        for model in manifest::MODELS {
            assert!(
                out.contains(model.id),
                "{} missing from the listing",
                model.id
            );
        }
    }
}
