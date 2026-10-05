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

    // The alias first, because it is the name clients use; the id beside it,
    // because it is the one that never changes.
    let alias_width = store
        .models()
        .filter_map(|model| model.alias.as_deref())
        .map(|alias| alias.chars().count())
        .max()
        .unwrap_or(0)
        .clamp("ALIAS".len(), lightweight_catalog::alias::MAX_ALIAS_CHARS);
    let _ = writeln!(
        out,
        "{:<alias_width$}  {:<32} {:>9}  {:<10} {:<9} INTEGRITY",
        "ALIAS", "ID", "SIZE", "ARCH", "CONTEXT"
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
            "{:<alias_width$}  {:<32} {:>9}  {:<10} {:<9} {}{}",
            model.alias.as_deref().unwrap_or("—"),
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
) -> Result<InstalledModel, String> {
    let installer = installer(paths)?;
    let (tx, reporter) = reporter("hashing");
    let model = installer
        .import(store, path, &tx)
        .await
        .map_err(crate::serve::describe)?;
    drop(tx);
    let _ = reporter.await;

    describe_added(out, &model);
    Ok(model)
}

/// `hermes models add <id>` and `hermes models add --url <link>`.
pub async fn add(
    out: &mut String,
    paths: &DataPaths,
    store: &mut CatalogStore,
    request: &AddModel,
) -> Result<InstalledModel, String> {
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
    Ok(model)
}

/// An alias asked for with `--alias`, checked before anything is fetched.
///
/// A name that is invalid or already taken is reported now, not after a
/// multi-gigabyte download has finished.
pub fn check_new_alias(
    store: &CatalogStore,
    alias: Option<&str>,
) -> Result<Option<String>, String> {
    let Some(raw) = alias else {
        return Ok(None);
    };
    store
        .check_alias(raw, None)
        .map(Some)
        .map_err(crate::serve::describe)
}

/// Give a model that was just added the alias asked for with `--alias`.
pub fn name_added(
    out: &mut String,
    store: &mut CatalogStore,
    added: &InstalledModel,
    alias: Option<&str>,
) -> Result<(), String> {
    match alias {
        Some(alias) => set_alias(out, store, &added.id, Some(alias)),
        None => Ok(()),
    }
}

/// How long to wait for a local gateway before concluding there is none.
///
/// Loopback, so a gateway that is there answers in milliseconds; this only
/// bounds the case where something holds the port and never replies.
const GATEWAY_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// What is listening where a gateway serving this profile would be.
#[derive(Debug, PartialEq, Eq)]
pub enum LocalGateway {
    /// Nothing answered: the catalog file is nobody else's right now.
    Absent,
    /// A gateway serving this very catalog, at this base URL. It owns the
    /// catalog in memory and will overwrite the file on its next save, so an
    /// alias must be changed through it.
    Ours(String),
    /// A gateway serving a *different* profile. Its catalog is not this one.
    OtherProfile,
}

/// Look for a gateway serving this profile on `127.0.0.1:port`.
///
/// No discovery beyond the one port a gateway with this profile's bind
/// settings would use: `GET /api/v1/gateway` reports the data directory it
/// serves, which is compared with ours. Loopback control requests need no key.
/// Anything that answers but cannot be read as a gateway report is an error,
/// because writing the file under a process that may own it is the outcome
/// this exists to avoid.
pub async fn local_gateway(paths: &DataPaths, port: u16) -> Result<LocalGateway, String> {
    if port == 0 {
        return Ok(LocalGateway::Absent);
    }
    let base = format!("http://127.0.0.1:{port}");
    let client = lightweight_download::client("lightweight-cli").map_err(crate::serve::describe)?;
    let response = match client
        .get(format!("{base}/api/v1/gateway"))
        .timeout(GATEWAY_PROBE_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        // Refused or silent: no gateway is serving there.
        Err(err) if err.is_connect() => return Ok(LocalGateway::Absent),
        Err(err) => return Err(gateway_in_the_way(port, &err.to_string())),
    };
    let status = response.status();
    let report: Option<serde_json::Value> = if status.is_success() {
        response.json().await.ok()
    } else {
        None
    };
    let Some(data) = report
        .as_ref()
        .and_then(|report| report["paths"]["data"].as_str())
    else {
        // Something holds the port without describing a profile. The default
        // port is also Ollama's, so it is only in the way if it says it is a
        // Lightweight gateway — one demanding a key, or an older build.
        return if is_a_lightweight_gateway(&client, &base).await {
            Err(gateway_in_the_way(port, &format!("it answered {status}")))
        } else {
            Ok(LocalGateway::Absent)
        };
    };
    if same_dir(Path::new(data), paths.data_dir()) {
        Ok(LocalGateway::Ours(base))
    } else {
        Ok(LocalGateway::OtherProfile)
    }
}

/// Whether whatever is at `base` says it is a Lightweight gateway.
///
/// `/version` is answered unauthenticated by every build of the gateway, with a
/// `build` that has started `hermes-gateway-` since M3.
async fn is_a_lightweight_gateway(client: &reqwest::Client, base: &str) -> bool {
    let Ok(response) = client
        .get(format!("{base}/version"))
        .timeout(GATEWAY_PROBE_TIMEOUT)
        .send()
        .await
    else {
        return false;
    };
    response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|body| {
            body["build"]
                .as_str()
                .map(|build| build.starts_with("hermes-gateway-"))
        })
        .unwrap_or(false)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

fn gateway_in_the_way(port: u16, why: &str) -> String {
    format!(
        "A Lightweight gateway appears to be running on 127.0.0.1:{port}, but this command \
         could not confirm which catalog it serves ({why}).\n\
         Use the panel or PATCH /api/v1/models/<id> to change aliases, or stop the gateway \
         before modifying the catalog with the CLI. The alias was not changed."
    )
}

/// `hermes models alias`, made through the running gateway when there is one.
///
/// A gateway holds the catalog in memory and saves it whole, so a change
/// written under it would be invisible to it now and overwritten by its next
/// save. When it is serving this profile the change goes through its control
/// API instead, which validates, persists and applies it at once. Otherwise the
/// file is changed directly, and the output says that is what happened.
pub async fn alias_command(
    out: &mut String,
    paths: &DataPaths,
    port: u16,
    store: &mut CatalogStore,
    model: &str,
    alias: Option<&str>,
) -> Result<(), String> {
    match local_gateway(paths, port).await? {
        LocalGateway::Ours(base) => alias_through_gateway(out, &base, model, alias).await,
        LocalGateway::Absent | LocalGateway::OtherProfile => {
            self::alias(out, store, model, alias)?;
            let _ = writeln!(
                out,
                "  changed in {} directly: no gateway serving this profile answered on \
                 127.0.0.1:{port}. One running on another port must be restarted to see it.",
                store.path().display()
            );
            Ok(())
        }
    }
}

async fn alias_through_gateway(
    out: &mut String,
    base: &str,
    model: &str,
    alias: Option<&str>,
) -> Result<(), String> {
    let mut url = reqwest::Url::parse(base).map_err(|err| err.to_string())?;
    url.path_segments_mut()
        .map_err(|()| format!("{base} cannot carry a path"))?
        .extend(["api", "v1", "models", model]);
    let client = lightweight_download::client("lightweight-cli").map_err(crate::serve::describe)?;
    let response = client
        .patch(url)
        .json(&serde_json::json!({ "alias": alias }))
        .timeout(GATEWAY_PROBE_TIMEOUT * 5)
        .send()
        .await
        .map_err(|err| {
            format!("the gateway at {base} stopped answering ({err}); the alias was not changed")
        })?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or_default();
    if !status.is_success() {
        // The gateway's own sentence: it is the authority on why.
        return Err(body["error"]["message"].as_str().map_or_else(
            || format!("the gateway refused the change ({status})"),
            str::to_owned,
        ));
    }

    let id = body["id"].as_str().unwrap_or(model);
    match body["alias"].as_str() {
        Some(alias) => {
            let _ = writeln!(out, "{id} is now served as {alias:?}");
        }
        None => {
            let _ = writeln!(out, "{id} has no alias; it is served under its id");
        }
    }
    let _ = writeln!(
        out,
        "  changed through the running gateway at {base}, which saved it and applies it now"
    );
    Ok(())
}

/// `hermes models alias <model> <alias>` and `hermes models alias <model> --clear`.
///
/// `model` may be the id or the current alias.
pub fn alias(
    out: &mut String,
    store: &mut CatalogStore,
    model: &str,
    alias: Option<&str>,
) -> Result<(), String> {
    let id = store
        .resolve(model)
        .map(|found| found.id.clone())
        .ok_or_else(|| {
            crate::serve::describe(lightweight_catalog::CatalogError::UnknownModel {
                id: model.to_owned(),
            })
        })?;
    set_alias(out, store, &id, alias)
}

fn set_alias(
    out: &mut String,
    store: &mut CatalogStore,
    id: &str,
    alias: Option<&str>,
) -> Result<(), String> {
    let previous = store.get(id).and_then(|model| model.alias.clone());
    store.set_alias(id, alias).map_err(crate::serve::describe)?;
    store.save().map_err(crate::serve::describe)?;

    match (previous.as_deref(), alias.map(str::trim)) {
        (_, Some(alias)) => {
            let _ = writeln!(out, "{id} is now served as {alias:?}");
            if let Some(previous) = previous.filter(|previous| previous != alias) {
                let _ = writeln!(out, "  {previous:?} no longer names it");
            }
        }
        (Some(previous), None) => {
            let _ = writeln!(
                out,
                "cleared the alias {previous:?}; {id} is served under its id again"
            );
        }
        (None, None) => {
            let _ = writeln!(out, "{id} had no alias");
        }
    }
    Ok(())
}

/// `hermes models remove <id>`.
pub fn remove(
    out: &mut String,
    store: &mut CatalogStore,
    id: &str,
    delete_file: bool,
) -> Result<(), String> {
    // By id or by alias; the alias goes with the record either way.
    let id = store
        .resolve(id)
        .map_or_else(|| id.to_owned(), |found| found.id.clone());
    let model = store.remove(&id).map_err(crate::serve::describe)?;
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
    if let Some(alias) = &model.alias {
        let _ = writeln!(out, "  alias      {alias}");
    }
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

    fn catalog_with(ids: &[&str]) -> CatalogStore {
        let mut store = CatalogStore::in_memory();
        for (n, id) in ids.iter().enumerate() {
            let record: InstalledModel = serde_json::from_value(serde_json::json!({
                "id": id, "name": id, "path": format!("/models/{id}.gguf"),
                "bytes": 1024, "sha256": format!("{n:02}"), "integrity": "imported",
                "source": {"kind": "import", "original_path": format!("/models/{id}.gguf")},
                "architecture": "llama", "supported": true, "added_at": 0
            }))
            .expect("record");
            store.insert(record).expect("insert");
        }
        store
    }

    #[test]
    fn the_listing_shows_the_alias_beside_the_id_and_a_dash_without_one() {
        let mut store = catalog_with(&["qwen3.5-9b-fable-5-v1-q8_0", "existing-model-q4_k_m"]);
        store
            .set_alias("qwen3.5-9b-fable-5-v1-q8_0", Some("Coder"))
            .expect("alias");
        let mut out = String::new();
        list(&mut out, &store);

        let header = out.lines().next().expect("a header");
        assert!(header.starts_with("ALIAS"), "{out}");
        assert!(header.contains("ID"), "{out}");
        let coder = out
            .lines()
            .find(|line| line.contains("qwen3.5-9b-fable-5-v1-q8_0"))
            .expect("the aliased row");
        assert!(coder.starts_with("Coder "), "{out}");
        let plain = out
            .lines()
            .find(|line| line.contains("existing-model-q4_k_m"))
            .expect("the unaliased row");
        assert!(plain.starts_with('—'), "{out}");
    }

    #[test]
    fn an_alias_is_set_renamed_and_cleared_by_id_or_by_alias() {
        let mut store = catalog_with(&["qwen3.5-9b-fable-5-v1-q8_0"]);
        let id = "qwen3.5-9b-fable-5-v1-q8_0";
        let mut out = String::new();

        alias(&mut out, &mut store, id, Some("Coder")).expect("set");
        assert_eq!(store.resolve("Coder").map(|m| m.id.as_str()), Some(id));

        alias(&mut out, &mut store, "coder", Some("Programming")).expect("rename by alias");
        assert!(store.resolve("Coder").is_none());
        assert!(out.contains("\"Coder\" no longer names it"), "{out}");

        alias(&mut out, &mut store, "Programming", None).expect("clear");
        assert_eq!(store.get(id).and_then(|m| m.alias.clone()), None);
        assert!(alias(&mut out, &mut store, "ghost", Some("X")).is_err());
    }

    #[test]
    fn a_taken_or_reserved_alias_is_refused_before_anything_is_added() {
        let mut store = catalog_with(&["a", "b"]);
        store.set_alias("a", Some("Coder")).expect("alias");
        assert!(check_new_alias(&store, Some("CODER")).is_err());
        assert!(check_new_alias(&store, Some("default")).is_err());
        assert!(
            check_new_alias(&store, Some("b")).is_err(),
            "another model's id"
        );
        assert_eq!(
            check_new_alias(&store, Some(" Research ")),
            Ok(Some("Research".to_owned()))
        );
        assert_eq!(check_new_alias(&store, None), Ok(None));
    }

    #[test]
    fn removing_by_alias_removes_the_model_and_its_name() {
        let mut store = catalog_with(&["a"]);
        store.set_alias("a", Some("Fast")).expect("alias");
        let mut out = String::new();
        remove(&mut out, &mut store, "Fast", false).expect("remove");
        assert!(store.is_empty());
        assert!(store.resolve("Fast").is_none());
    }

    mod running_gateway {
        use super::super::*;
        use std::sync::Arc;

        use lightweight_backend_mock::MockBackend;
        use lightweight_gateway::manager::{ModelManager, RuntimeDefaults};
        use lightweight_gateway::{GatewayConfig, GatewayState};
        use lightweight_gguf::fixture::{GgufBuilder, TempDir};

        /// A profile with one imported model, and the id it was given.
        async fn profile(tag: &str) -> (TempDir, DataPaths, String) {
            let dir = TempDir::new(tag);
            let paths = DataPaths::rooted_at(dir.path());
            let file = dir.write(
                "Qwen3-8B-Q4_K_M.gguf",
                &GgufBuilder::small_model("llama").build(),
            );
            let mut store = CatalogStore::open(paths.catalog_file()).expect("catalog");
            let installer =
                Installer::new(paths.models_dir(), paths.downloads_dir()).expect("installer");
            let (tx, _rx) = mpsc::channel(64);
            let id = installer
                .import(&mut store, &file, &tx)
                .await
                .expect("import")
                .id;
            (dir, paths, id)
        }

        /// A real gateway over `paths`' catalog, as `hermes serve` builds one.
        async fn serve(paths: &DataPaths) -> (u16, Arc<GatewayState>) {
            let manager = Arc::new(ModelManager::new(
                CatalogStore::open(paths.catalog_file()).expect("catalog"),
                Installer::new(paths.models_dir(), paths.downloads_dir()).expect("installer"),
                RuntimeDefaults::default(),
            ));
            let state = Arc::new(
                GatewayState::new(
                    Arc::new(MockBackend::default()),
                    lightweight_gateway::catalog::shared(None),
                    GatewayConfig {
                        paths: Some(paths.clone()),
                        ..GatewayConfig::default()
                    },
                )
                .with_manager(manager),
            );
            let app = lightweight_gateway::app(Arc::clone(&state));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            tokio::spawn(async move {
                let _ = axum::serve(listener, lightweight_gateway::service(app)).await;
            });
            (port, state)
        }

        /// A port nothing is listening on.
        async fn closed_port() -> u16 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            listener.local_addr().expect("addr").port()
        }

        fn alias_on_disk(paths: &DataPaths, id: &str) -> Option<String> {
            CatalogStore::open(paths.catalog_file())
                .expect("catalog")
                .get(id)
                .and_then(|model| model.alias.clone())
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_gateway_serving_this_profile_makes_the_change_itself() {
            let (_dir, paths, id) = profile("cli-ours").await;
            let (port, state) = serve(&paths).await;
            let mut store = CatalogStore::open(paths.catalog_file()).expect("catalog");

            let mut out = String::new();
            alias_command(&mut out, &paths, port, &mut store, &id, Some("Coder"))
                .await
                .expect("through the gateway");
            assert!(out.contains("running gateway"), "{out}");

            // The running gateway knows it now, and saved it.
            let held = state.manager().expect("manager").resolve("Coder").await;
            assert_eq!(held.map(|model| model.id), Some(id.clone()));
            assert_eq!(alias_on_disk(&paths, &id).as_deref(), Some("Coder"));

            // A refusal is the gateway's, reported as a failure, not a success.
            let err = alias_command(&mut out, &paths, port, &mut store, &id, Some("default"))
                .await
                .expect_err("reserved");
            assert!(err.contains("default"), "{err}");
            assert_eq!(alias_on_disk(&paths, &id).as_deref(), Some("Coder"));

            // Cleared through it too, by the alias it currently has.
            alias_command(&mut out, &paths, port, &mut store, "Coder", None)
                .await
                .expect("clear");
            assert_eq!(alias_on_disk(&paths, &id), None);
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn with_no_gateway_the_file_is_changed_and_the_output_says_so() {
            let (_dir, paths, id) = profile("cli-absent").await;
            let mut store = CatalogStore::open(paths.catalog_file()).expect("catalog");
            let mut out = String::new();
            alias_command(
                &mut out,
                &paths,
                closed_port().await,
                &mut store,
                &id,
                Some("Coder"),
            )
            .await
            .expect("direct");
            assert!(out.contains("directly"), "{out}");
            assert_eq!(alias_on_disk(&paths, &id).as_deref(), Some("Coder"));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn a_gateway_for_another_profile_is_not_this_catalogs_owner() {
            let (_dir, paths, id) = profile("cli-mine").await;
            let (_other_dir, other_paths, _) = profile("cli-theirs").await;
            let (port, _state) = serve(&other_paths).await;
            let mut store = CatalogStore::open(paths.catalog_file()).expect("catalog");
            let mut out = String::new();
            alias_command(&mut out, &paths, port, &mut store, &id, Some("Coder"))
                .await
                .expect("direct");
            assert_eq!(alias_on_disk(&paths, &id).as_deref(), Some("Coder"));
            assert_eq!(alias_on_disk(&other_paths, &id), None, "theirs untouched");
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn another_kind_of_server_on_the_port_is_not_in_the_way() {
            // The default port is Ollama's too. Something that is not a
            // Lightweight gateway owns no catalog of ours.
            let (_dir, paths, id) = profile("cli-ollama").await;
            let app = axum::Router::new().route(
                "/api/version",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({ "version": "0.5" }))
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let mut store = CatalogStore::open(paths.catalog_file()).expect("catalog");
            let mut out = String::new();
            alias_command(&mut out, &paths, port, &mut store, &id, Some("Coder"))
                .await
                .expect("direct");
            assert!(out.contains("directly"), "{out}");
            assert_eq!(alias_on_disk(&paths, &id).as_deref(), Some("Coder"));
        }

        #[tokio::test(flavor = "multi_thread")]
        async fn something_that_cannot_be_confirmed_stops_the_write() {
            let (_dir, paths, id) = profile("cli-unknown").await;
            // Holds the port and refuses to say what it serves, as a gateway
            // demanding a key would.
            let app = axum::Router::new()
                .route(
                    "/api/v1/gateway",
                    axum::routing::get(|| async { axum::http::StatusCode::UNAUTHORIZED }),
                )
                .route(
                    "/version",
                    axum::routing::get(|| async {
                        axum::Json(serde_json::json!({ "build": "hermes-gateway-9.9.9" }))
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let mut store = CatalogStore::open(paths.catalog_file()).expect("catalog");
            let mut out = String::new();
            let err = alias_command(&mut out, &paths, port, &mut store, &id, Some("Coder"))
                .await
                .expect_err("must not write under an unknown gateway");
            assert!(err.contains("appears to be running"), "{err}");
            assert!(err.contains("stop the gateway"), "{err}");
            assert_eq!(alias_on_disk(&paths, &id), None, "nothing was written");
        }
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
