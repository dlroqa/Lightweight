//! `hermes fleet`: run up to a few isolated per-model gateways at once.
//!
//! The engine is single-resident by design, so the way to serve several models
//! — and to keep one tenant's traffic from evicting another's — is several
//! gateways, one per model, each re-rooted with its own `HERMES_GATEWAY_HOME`
//! so its keys, rate limits, catalog and settings are its own. This command
//! reads a small manifest, enforces the ceiling, and launches each entry as a
//! child `hermes serve … --behind-proxy`.
//!
//! Nothing here shares memory between models: each entry is a separate process
//! precisely so the isolation is the operating system's to enforce, not this
//! command's. It is the one place the model count is capped, so "why won't a
//! fifth start?" has a single answer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Deserialize;

use lightweight_system_info::DataPaths;
use lightweight_system_info::paths::HOME_ENV;

/// The most models one fleet may run at once.
///
/// Four is a deliberate ceiling, not an accident. Each model is a full engine
/// process with its own weights and KV cache resident, so the real limit is the
/// machine's memory, which four large models reach well before anything in
/// software does. Keeping it small also keeps the manifest something a person
/// can hold in their head. Raise it here, with a reason, if a machine genuinely
/// wants more — this is the one place the number lives.
pub const MAX_MODELS: usize = 4;

/// The manifest a fleet is described by.
#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    models: Vec<ModelEntry>,
}

/// One model in the manifest.
#[derive(Debug, Deserialize)]
struct ModelEntry {
    /// A short name, used for the default profile directory and in messages.
    name: String,
    /// The `.gguf` this gateway serves.
    path: PathBuf,
    /// The loopback port this gateway binds. Fixed, never `auto`: the tunnel's
    /// ingress rule points a hostname at this exact port.
    port: u16,
    /// The hostname the tunnel routes to this model. Advisory — used only to
    /// print the roster; the tunnel config itself lives in `cloudflared`.
    #[serde(default)]
    host: Option<String>,
    /// The data root (`HERMES_GATEWAY_HOME`) that isolates this model's keys,
    /// limits and catalog. Defaults to `<manifest dir>/<name>`.
    #[serde(default)]
    profile: Option<PathBuf>,
}

/// A manifest entry with its profile resolved to an absolute-enough path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedEntry {
    name: String,
    path: PathBuf,
    port: u16,
    host: Option<String>,
    profile: PathBuf,
}

/// Why a fleet manifest was rejected.
///
/// Typed rather than a bare string so the cap and the other guards can be
/// asserted on in tests without matching prose.
#[derive(Debug, PartialEq, Eq)]
enum FleetError {
    Empty,
    TooMany {
        count: usize,
    },
    DuplicateName {
        name: String,
    },
    DuplicatePort {
        port: u16,
        first: String,
        second: String,
    },
    DuplicateProfile {
        profile: PathBuf,
    },
    AutoPort {
        name: String,
    },
    MissingModel {
        name: String,
        path: PathBuf,
    },
    NoKey {
        name: String,
        profile: PathBuf,
    },
}

impl std::fmt::Display for FleetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the fleet manifest lists no models"),
            Self::TooMany { count } => write!(
                f,
                "a fleet is limited to {MAX_MODELS} models, but {count} were configured. \
                 Serve fewer, or run the extras as their own `hermes serve` on another host."
            ),
            Self::DuplicateName { name } => {
                write!(
                    f,
                    "two models share the name {name:?}; names must be unique"
                )
            }
            Self::DuplicatePort {
                port,
                first,
                second,
            } => write!(
                f,
                "{first} and {second} both bind port {port}; give each model its own port"
            ),
            Self::DuplicateProfile { profile } => write!(
                f,
                "two models share the profile {}; each model needs its own data root so its \
                 keys and limits stay separate",
                profile.display()
            ),
            Self::AutoPort { name } => write!(
                f,
                "{name} must bind a fixed port, not 0/auto: the tunnel routes a hostname to a \
                 known port"
            ),
            Self::MissingModel { name, path } => {
                write!(
                    f,
                    "the model file for {name} is missing: {}",
                    path.display()
                )
            }
            Self::NoKey { name, profile } => write!(
                f,
                "the profile for {name} has no API key, and a gateway behind a proxy needs one. \
                 Mint it with:\n\n    HERMES_GATEWAY_HOME={} hermes key create --name {name}",
                profile.display()
            ),
        }
    }
}

impl Manifest {
    /// Check the manifest and resolve each entry's profile.
    ///
    /// The order of the checks is deliberate: the ceiling is tested before any
    /// per-entry work, so a manifest that is simply too large says so rather
    /// than first complaining about the sixth entry's missing file.
    fn resolve(&self, manifest_dir: &Path) -> Result<Vec<ResolvedEntry>, FleetError> {
        if self.models.is_empty() {
            return Err(FleetError::Empty);
        }
        if self.models.len() > MAX_MODELS {
            return Err(FleetError::TooMany {
                count: self.models.len(),
            });
        }

        // Cheap, whole-manifest checks first, so a duplicate anywhere in the
        // list is reported before an earlier entry's missing file — a manifest
        // with a typo'd name and a not-yet-downloaded model should name the
        // typo, which is the thing the operator can fix without a download.
        let mut names: HashMap<&str, ()> = HashMap::new();
        let mut ports: HashMap<u16, &str> = HashMap::new();
        let mut profiles: HashMap<PathBuf, ()> = HashMap::new();
        let mut resolved = Vec::with_capacity(self.models.len());

        for entry in &self.models {
            if names.insert(entry.name.as_str(), ()).is_some() {
                return Err(FleetError::DuplicateName {
                    name: entry.name.clone(),
                });
            }
            if entry.port == 0 {
                return Err(FleetError::AutoPort {
                    name: entry.name.clone(),
                });
            }
            if let Some(first) = ports.insert(entry.port, entry.name.as_str()) {
                return Err(FleetError::DuplicatePort {
                    port: entry.port,
                    first: first.to_owned(),
                    second: entry.name.clone(),
                });
            }
            let profile = entry
                .profile
                .clone()
                .unwrap_or_else(|| manifest_dir.join(&entry.name));
            if profiles.insert(profile.clone(), ()).is_some() {
                return Err(FleetError::DuplicateProfile { profile });
            }
            resolved.push(ResolvedEntry {
                name: entry.name.clone(),
                path: entry.path.clone(),
                port: entry.port,
                host: entry.host.clone(),
                profile,
            });
        }

        // Only once the manifest is internally consistent do we touch the disk.
        for entry in &resolved {
            if !entry.path.is_file() {
                return Err(FleetError::MissingModel {
                    name: entry.name.clone(),
                    path: entry.path.clone(),
                });
            }
        }
        Ok(resolved)
    }
}

/// Refuse to launch a gateway whose profile has no key.
///
/// `--behind-proxy` demands a credential, and the gateway would refuse to start
/// without one — but discovering that after three of four models are already up
/// is a worse failure than saying it here, before anything is launched.
fn check_keys(entries: &[ResolvedEntry]) -> Result<(), FleetError> {
    for entry in entries {
        let paths = DataPaths::rooted_at(&entry.profile);
        let store = lightweight_store::ApiKeyStore::new(paths.api_keys_file());
        if !store.any().unwrap_or(false) {
            return Err(FleetError::NoKey {
                name: entry.name.clone(),
                profile: entry.profile.clone(),
            });
        }
    }
    Ok(())
}

/// Where the manifest lives when `--config` was not given.
fn default_config_path() -> Result<PathBuf, String> {
    let paths = DataPaths::discover().map_err(crate::serve::describe)?;
    Ok(paths.config_dir().join("fleet.json"))
}

/// Run the fleet described by the manifest at `config`, or the default path.
pub fn run(config: Option<PathBuf>) -> Result<ExitCode, String> {
    let path = match config {
        Some(path) => path,
        None => default_config_path()?,
    };
    let text = std::fs::read_to_string(&path).map_err(|err| {
        format!(
            "the fleet manifest {} could not be read: {err}. \
             Write one, or pass --config <path>.",
            path.display()
        )
    })?;
    let manifest: Manifest = serde_json::from_str(&text).map_err(|err| {
        format!(
            "the fleet manifest {} is not valid JSON: {err}",
            path.display()
        )
    })?;
    let manifest_dir = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let entries = manifest
        .resolve(&manifest_dir)
        .map_err(|err| err.to_string())?;
    check_keys(&entries).map_err(|err| err.to_string())?;

    let runtime = crate::runtime()?;
    runtime.block_on(supervise(entries))
}

/// Launch every gateway and wait until a stop signal, then reap them.
///
/// The children share this process's group, so an interactive Ctrl-C — and a
/// service manager's SIGTERM under the default cgroup kill — reaches each
/// `hermes serve` directly and it shuts its own engine down cleanly. This
/// parent's job is therefore to wait for the signal and then wait for the
/// children to finish, rather than to kill them out from under their cleanup.
/// Auto-restart of a gateway that dies on its own is deliberately left out of
/// this first version.
async fn supervise(entries: Vec<ResolvedEntry>) -> Result<ExitCode, String> {
    let exe = std::env::current_exe()
        .map_err(|err| format!("could not find the running executable to launch: {err}"))?;

    let mut children = Vec::with_capacity(entries.len());
    for entry in &entries {
        let mut command = tokio::process::Command::new(&exe);
        command
            .arg("serve")
            .arg(&entry.path)
            .arg("--port")
            .arg(entry.port.to_string())
            .arg("--behind-proxy")
            .env(HOME_ENV, &entry.profile);
        let child = command
            .spawn()
            .map_err(|err| format!("could not start the gateway for {}: {err}", entry.name))?;
        children.push((entry.name.clone(), child));
    }

    println!("fleet: {} models", entries.len());
    for entry in &entries {
        match &entry.host {
            Some(host) => println!(
                "  {:<16} http://localhost:{}  →  https://{host}/v1",
                entry.name, entry.port
            ),
            None => println!("  {:<16} http://localhost:{}/v1", entry.name, entry.port),
        }
    }
    println!("\nPress Ctrl-C to stop the fleet.");

    wait_for_stop().await;
    println!("\nstopping the fleet; waiting for gateways to shut down");
    for (name, mut child) in children {
        if let Err(err) = child.wait().await {
            eprintln!("  {name}: {err}");
        }
    }
    println!("fleet stopped");
    Ok(ExitCode::SUCCESS)
}

/// Resolve when the fleet should stop: Ctrl-C, or SIGTERM on a unix host.
async fn wait_for_stop() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            // No SIGTERM handler: Ctrl-C alone still stops the fleet.
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::write(path, b"gguf").expect("write temp model");
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-fleet-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir temp");
        dir
    }

    fn entry(name: &str, path: &Path, port: u16) -> ModelEntry {
        ModelEntry {
            name: name.to_owned(),
            path: path.to_path_buf(),
            port,
            host: None,
            profile: None,
        }
    }

    #[test]
    fn a_fifth_model_is_refused_before_anything_launches() {
        // The cap is checked ahead of any per-entry work, so five entries fail
        // as "too many" even with paths that do not exist.
        let manifest = Manifest {
            models: (0..5)
                .map(|i| entry(&format!("m{i}"), Path::new("/nope.gguf"), 11434 + i as u16))
                .collect(),
        };
        assert_eq!(
            manifest.resolve(Path::new("/tmp")),
            Err(FleetError::TooMany { count: 5 })
        );
    }

    #[test]
    fn four_models_are_allowed() {
        let dir = temp_dir("ok");
        let models: Vec<ModelEntry> = (0..4)
            .map(|i| {
                let path = dir.join(format!("m{i}.gguf"));
                touch(&path);
                entry(&format!("m{i}"), &path, 11434 + i as u16)
            })
            .collect();
        let manifest = Manifest { models };
        let resolved = manifest.resolve(&dir).expect("four models fit");
        assert_eq!(resolved.len(), 4);
        // The profile defaults to a sibling of the manifest named for the model.
        assert_eq!(resolved[0].profile, dir.join("m0"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_manifest_is_refused() {
        let manifest = Manifest { models: vec![] };
        assert_eq!(manifest.resolve(Path::new("/tmp")), Err(FleetError::Empty));
    }

    #[test]
    fn two_models_on_one_port_are_refused() {
        let manifest = Manifest {
            models: vec![
                entry("a", Path::new("/a.gguf"), 11434),
                entry("b", Path::new("/b.gguf"), 11434),
            ],
        };
        assert!(matches!(
            manifest.resolve(Path::new("/tmp")),
            Err(FleetError::DuplicatePort { port: 11434, .. })
        ));
    }

    #[test]
    fn two_models_with_one_name_are_refused() {
        let manifest = Manifest {
            models: vec![
                entry("same", Path::new("/a.gguf"), 11434),
                entry("same", Path::new("/b.gguf"), 11435),
            ],
        };
        assert_eq!(
            manifest.resolve(Path::new("/tmp")),
            Err(FleetError::DuplicateName {
                name: "same".to_owned()
            })
        );
    }

    #[test]
    fn an_auto_port_is_refused() {
        let manifest = Manifest {
            models: vec![entry("a", Path::new("/a.gguf"), 0)],
        };
        assert_eq!(
            manifest.resolve(Path::new("/tmp")),
            Err(FleetError::AutoPort {
                name: "a".to_owned()
            })
        );
    }

    #[test]
    fn a_missing_model_file_is_refused() {
        let manifest = Manifest {
            models: vec![entry("a", Path::new("/definitely/not/here.gguf"), 11434)],
        };
        assert!(matches!(
            manifest.resolve(Path::new("/tmp")),
            Err(FleetError::MissingModel { .. })
        ));
    }

    #[test]
    fn a_profile_without_a_key_is_refused() {
        // The preflight that spares the operator a half-launched fleet: a
        // profile with no keystore has no credential, and a proxied gateway
        // needs one.
        let dir = temp_dir("nokey");
        let model = dir.join("m.gguf");
        touch(&model);
        let resolved = vec![ResolvedEntry {
            name: "a".to_owned(),
            path: model,
            port: 11434,
            host: None,
            profile: dir.join("empty-profile"),
        }];
        assert!(matches!(
            check_keys(&resolved),
            Err(FleetError::NoKey { .. })
        ));
        std::fs::remove_dir_all(&dir).ok();
    }
}
