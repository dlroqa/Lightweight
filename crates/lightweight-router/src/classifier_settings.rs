//! Saving the classifier's provider settings — and the Jev key — from the
//! panel, without editing `router.json` by hand.
//!
//! The smallest write the router has ever offered, and nothing more:
//!
//! * `GET  /api/router/v1/classifier/settings` — what is saved, what is
//!   running, where the key comes from, and whether a restart is pending.
//!   Read under the router's ordinary credential, like `/auto`. Never a key.
//! * `PUT  /api/router/v1/classifier/settings` — set `provider` and the Jev
//!   block's `base_url`, `model`, `timeout_ms`, `min_confidence`; optionally
//!   replace the key. An empty key leaves the saved one as it is.
//! * `DELETE /api/router/v1/classifier/key` — remove the saved key.
//!
//! Writes need the admin token and a same-origin loopback request
//! ([`crate::admin`]), and `If-Match` with the revision last read, so two
//! panels — or a panel and a text editor — cannot overwrite each other.
//!
//! Only `auto_route.classifier` changes. The rest of the file is kept as
//! written, key order included; the whole result is validated exactly as a
//! start would validate it before anything is written. The key goes to the
//! operating system's credential store ([`crate::secret_store`]), never to the
//! file; a failed file write puts the previous key back.
//!
//! Nothing is applied to the running router. The router reads its
//! configuration once, at start; a save reports `restart_required`, and Test
//! Connection keeps checking the settings that are running.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use indexmap::IndexMap;
use lightweight_api::error::ErrorEnvelope;
use lightweight_observability::targets;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::RouterState;
use crate::admin::{AdminAccess, MAX_BODY_BYTES};
use crate::classifier::ProviderKind;
use crate::classifier::jev::{DEFAULT_API_KEY_ENV, DEFAULT_BASE_URL, KeySource};
use crate::config::RouterFile;
use crate::secret_store::{SecretStore, StoreFailure, jev_account};

/// The longest key accepted. TypeSafe's are far shorter.
const MAX_KEY_CHARS: usize = 4096;

/// How the process environment is read: a parameter so tests need not
/// mutate the real one.
pub type Env = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// The settings writer of one running router.
pub struct ClassifierSettings {
    path: PathBuf,
    /// `auto_route.classifier` as the router started with it.
    active: Option<Value>,
    store: Arc<dyn SecretStore>,
    env: Env,
    admin: Result<AdminAccess, String>,
    write: tokio::sync::Mutex<()>,
    /// Set once this process has replaced or removed the saved key.
    key_changed: AtomicBool,
    /// Test-only: fail the next file write after the key is stored.
    #[doc(hidden)]
    pub fail_next_write: AtomicBool,
}

impl std::fmt::Debug for ClassifierSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClassifierSettings")
            .field("path", &self.path)
            .field("store", &self.store.backend())
            .field("admin", &self.admin.is_ok())
            .finish_non_exhaustive()
    }
}

impl ClassifierSettings {
    /// `loaded` is the text the running configuration came from.
    pub fn new(
        path: PathBuf,
        loaded: &str,
        store: Arc<dyn SecretStore>,
        env: Env,
        admin: Result<AdminAccess, String>,
    ) -> Self {
        Self {
            path,
            active: section_of(loaded),
            store,
            env,
            admin,
            write: tokio::sync::Mutex::new(()),
            key_changed: AtomicBool::new(false),
            fail_next_write: AtomicBool::new(false),
        }
    }

    fn read(&self) -> Result<(String, String), Failure> {
        let text = std::fs::read_to_string(&self.path).map_err(|_| {
            Failure::new(
                StatusCode::CONFLICT,
                "config_unreadable",
                "the router's configuration file cannot be read",
            )
        })?;
        let revision = revision(&text);
        Ok((text, revision))
    }

    /// Whether a key is in the environment and in the store, and which wins.
    fn key_status(&self, api_key_env: &str) -> Value {
        let environment = (self.env)(api_key_env).is_some_and(|value| !value.trim().is_empty());
        let stored = self.store.get(&jev_account(api_key_env));
        let (in_store, store_view) = match &stored {
            Ok(found) => (
                Some(found.is_some()),
                json!({"available": true, "backend": self.store.backend()}),
            ),
            Err(failure) => (
                None,
                json!({
                    "available": !matches!(failure, StoreFailure::Unavailable(_)),
                    "backend": self.store.backend(),
                    "detail": failure.message(),
                }),
            ),
        };
        let source = if environment {
            KeySource::Environment
        } else if in_store == Some(true) {
            KeySource::CredentialStore
        } else {
            KeySource::Missing
        };
        json!({
            "api_key_env": api_key_env,
            "source": source.as_str(),
            "environment": environment,
            "stored": in_store,
            "store": store_view,
        })
    }

    /// The panel's view. Blocking: it reads the file and asks the store.
    fn snapshot(&self, state: &RouterState) -> Result<Value, Failure> {
        let (text, revision) = self.read()?;
        let saved = section_of(&text);
        let saved_summary = summary(saved.as_ref());
        let api_key_env = saved_summary["jev"]["api_key_env"]
            .as_str()
            .unwrap_or(DEFAULT_API_KEY_ENV)
            .to_owned();

        let settings_changed = saved != self.active;
        let key_changed = self.key_changed.load(Ordering::SeqCst);
        let mut reasons = Vec::new();
        if settings_changed {
            reasons.push("settings_changed");
        }
        if key_changed {
            reasons.push("key_changed");
        }

        let mut active = summary(self.active.as_ref());
        active["key_source"] = json!(running_jev(state).map(|jev| jev.key_source.as_str()));

        Ok(json!({
            "file": self.path.file_name().map(|name| name.to_string_lossy().into_owned()),
            "revision": revision,
            "configured": saved.is_some(),
            "providers": ["lightweight", "jev"],
            "saved": saved_summary,
            "active": active,
            "restart_required": settings_changed || key_changed,
            "restart_reasons": reasons,
            "key": self.key_status(&api_key_env),
            "admin": match &self.admin {
                Ok(_) => json!({"available": true, "token_command": "hermes router admin-token"}),
                Err(reason) => json!({"available": false, "detail": reason}),
            },
        }))
    }

    /// Save, under the write lock. Blocking.
    fn save(&self, expected: &str, request: SaveRequest) -> Result<&'static str, Failure> {
        let (text, current) = self.read()?;
        if current != expected {
            return Err(Failure::conflict(&current));
        }
        let mut document: Ordered = serde_json::from_str(&text).map_err(|_| {
            Failure::new(
                StatusCode::CONFLICT,
                "config_unreadable",
                "the configuration file is not valid JSON; fix it by hand first",
            )
        })?;
        let section = classifier_section(&mut document)?;
        section.insert(
            "provider".into(),
            Ordered::String(request.provider.as_str().into()),
        );
        if let Some(patch) = &request.jev {
            let block = match section
                .entry("jev".into())
                .or_insert_with(|| Ordered::Object(IndexMap::new()))
            {
                Ordered::Object(block) => block,
                _ => {
                    return Err(Failure::new(
                        StatusCode::CONFLICT,
                        "config_unreadable",
                        "auto_route.classifier.jev is not an object; fix it by hand first",
                    ));
                }
            };
            block.insert(
                "base_url".into(),
                Ordered::String(patch.base_url.trim().into()),
            );
            block.insert("model".into(), Ordered::String(patch.model.trim().into()));
            if let Some(ms) = patch.timeout_ms {
                block.insert("timeout_ms".into(), Ordered::Number(ms.into()));
            }
            if let Some(confidence) = patch.min_confidence {
                let number = serde_json::Number::from_f64(confidence).ok_or_else(|| {
                    Failure::invalid("min_confidence must be a number between 0 and 1")
                })?;
                block.insert("min_confidence".into(), Ordered::Number(number));
            }
        }
        let api_key_env = match section.get("jev") {
            Some(Ordered::Object(block)) => match block.get("api_key_env") {
                Some(Ordered::String(name)) => name.clone(),
                _ => DEFAULT_API_KEY_ENV.to_owned(),
            },
            _ => DEFAULT_API_KEY_ENV.to_owned(),
        };
        let mut next = serde_json::to_string_pretty(&document).map_err(|_| {
            Failure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "the new configuration could not be written out",
            )
        })?;
        next.push('\n');

        // Validate the whole file exactly as a start would, with the new key
        // standing in for the stored one.
        let new_key = request.api_key.as_deref();
        let environment = (self.env)(&api_key_env).is_some_and(|value| !value.trim().is_empty());
        let file: RouterFile = serde_json::from_str(&next)
            .map_err(|err| Failure::invalid_with(vec![err.to_string()]))?;
        let stored = |var: &str| {
            if var == api_key_env
                && let Some(key) = new_key
            {
                return Some(key.to_owned());
            }
            self.store
                .get(&jev_account(var))
                .ok()
                .flatten()
                .map(|secret| secret.expose().to_owned())
        };
        let env = |var: &str| (self.env)(var);
        if request.provider == ProviderKind::Jev && new_key.is_none() && !environment {
            let held = self
                .store
                .get(&jev_account(&api_key_env))
                .map_err(|failure| store_failure(&failure))?;
            if held.is_none() {
                return Err(Failure::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "api_key_missing",
                    "Jev needs an API key: enter one to save it to this machine's credential \
                     store, or set the environment variable where the router runs",
                ));
            }
        }
        crate::config::validate_with_store(file, &env, &stored).map_err(|errors| {
            Failure::invalid_with(errors.0.iter().map(ToString::to_string).collect())
        })?;

        // The key first, so the file never names a provider whose key is not
        // there; and back out of it if the file cannot be written.
        let account = jev_account(&api_key_env);
        let previous = match new_key {
            Some(key) => {
                let previous = self
                    .store
                    .get(&account)
                    .map_err(|failure| store_failure(&failure))?;
                self.store
                    .set(&account, key)
                    .map_err(|failure| store_failure(&failure))?;
                let readback = self
                    .store
                    .get(&account)
                    .map_err(|failure| store_failure(&failure))?;
                if readback.as_ref().map(crate::domain::Secret::expose) != Some(key) {
                    restore(self.store.as_ref(), &account, previous.as_ref());
                    return Err(Failure::new(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "credential_store_failed",
                        "the credential store did not keep the key; nothing was saved",
                    ));
                }
                Some(previous)
            }
            None => None,
        };

        let written = if self.fail_next_write.swap(false, Ordering::SeqCst) {
            Err(std::io::Error::other("injected failure"))
        } else {
            write_atomically(&backup_path(&self.path), text.as_bytes())
                .and_then(|()| write_atomically(&self.path, next.as_bytes()))
        };
        if written.is_err() {
            if let Some(previous) = previous {
                restore(self.store.as_ref(), &account, previous.as_ref());
            }
            return Err(Failure::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "save_failed",
                "the configuration file could not be written; the previous settings and key \
                 are unchanged",
            ));
        }
        if new_key.is_some() {
            self.key_changed.store(true, Ordering::SeqCst);
        }
        tracing::info!(
            target: targets::ROUTER,
            action = "classifier_settings_saved",
            provider = request.provider.as_str(),
            model = request.jev.as_ref().map(|jev| jev.model.trim()),
            base_url = request.jev.as_ref().map(|jev| jev.base_url.trim()),
            key = if new_key.is_some() { "replaced" } else { "unchanged" },
            revision = &revision(&next)[..12],
            "classifier settings saved; restart the router to apply them"
        );
        Ok(if new_key.is_some() {
            "replaced"
        } else {
            "unchanged"
        })
    }

    /// Remove the saved key, unless the saved settings depend on it. Blocking.
    fn remove_key(&self, expected: &str) -> Result<(), Failure> {
        let (text, current) = self.read()?;
        if current != expected {
            return Err(Failure::conflict(&current));
        }
        let saved = summary(section_of(&text).as_ref());
        let api_key_env = saved["jev"]["api_key_env"]
            .as_str()
            .unwrap_or(DEFAULT_API_KEY_ENV)
            .to_owned();
        let environment = (self.env)(&api_key_env).is_some_and(|value| !value.trim().is_empty());
        if saved["provider"] == "jev" && !environment {
            return Err(Failure::new(
                StatusCode::CONFLICT,
                "key_in_use",
                "the saved settings use Jev with this key; switch the provider or set the \
                 environment variable first, so the router can still start",
            ));
        }
        self.store
            .delete(&jev_account(&api_key_env))
            .map_err(|failure| store_failure(&failure))?;
        self.key_changed.store(true, Ordering::SeqCst);
        tracing::info!(
            target: targets::ROUTER,
            action = "classifier_key_removed",
            api_key_env = api_key_env.as_str(),
            "saved classifier key removed from the credential store"
        );
        Ok(())
    }
}

/// The Jev provider the router is running with, active or standby.
fn running_jev(state: &RouterState) -> Option<&crate::classifier::jev::JevClassifier> {
    let classifier = state.auto.as_ref()?.classifier.as_ref()?;
    std::iter::once(&classifier.provider)
        .chain(classifier.standby.as_ref())
        .find_map(|provider| match provider {
            crate::classifier::ClassifierProvider::Jev(jev) => Some(jev),
            crate::classifier::ClassifierProvider::Lightweight(_) => None,
        })
}

fn restore(store: &dyn SecretStore, account: &str, previous: Option<&crate::domain::Secret>) {
    let restored = match previous {
        Some(secret) => store.set(account, secret.expose()),
        None => store.delete(account),
    };
    if restored.is_err() {
        tracing::error!(
            target: targets::ROUTER,
            action = "classifier_key_restore_failed",
            "the previous classifier key could not be put back in the credential store"
        );
    }
}

// --- the HTTP surface ------------------------------------------------------------------

/// What a save may change.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveRequest {
    provider: ProviderKind,
    #[serde(default)]
    jev: Option<JevPatch>,
    /// A replacement key. Absent or empty: the saved key is left as it is.
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JevPatch {
    base_url: String,
    model: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    min_confidence: Option<f64>,
}

/// A refused or failed request, as the router's JSON error.
#[derive(Debug)]
struct Failure {
    status: StatusCode,
    code: &'static str,
    message: String,
    errors: Vec<String>,
    revision: Option<String>,
}

impl Failure {
    fn new(status: StatusCode, code: &'static str, message: &str) -> Self {
        Self {
            status,
            code,
            message: message.to_owned(),
            errors: Vec::new(),
            revision: None,
        }
    }

    fn invalid(message: &str) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_settings",
            message,
        )
    }

    fn invalid_with(errors: Vec<String>) -> Self {
        let mut failure = Self::invalid("the router would refuse these settings");
        failure.errors = errors;
        failure
    }

    fn conflict(current: &str) -> Self {
        let mut failure = Self::new(
            StatusCode::PRECONDITION_FAILED,
            "revision_conflict",
            "the configuration changed since it was read; reload and try again",
        );
        failure.revision = Some(current.to_owned());
        failure
    }

    fn into_response(self) -> Response {
        let mut body = ErrorEnvelope::invalid_request(self.message, self.code).to_value();
        let mut envelope = json!({"error": body.take()});
        if !self.errors.is_empty() {
            envelope["errors"] = json!(self.errors);
        }
        if let Some(revision) = self.revision {
            envelope["revision"] = json!(revision);
        }
        (
            self.status,
            [(header::CONTENT_TYPE, "application/json")],
            envelope.to_string(),
        )
            .into_response()
    }
}

fn store_failure(failure: &StoreFailure) -> Failure {
    let status = match failure {
        StoreFailure::Unavailable(_) => StatusCode::CONFLICT,
        StoreFailure::Failed(_) => StatusCode::SERVICE_UNAVAILABLE,
    };
    let message = match failure {
        StoreFailure::Unavailable(reason) => format!(
            "{reason}. Nothing was saved. Set the key in the environment variable where the \
             router runs instead, then restart it"
        ),
        StoreFailure::Failed(reason) => format!("{reason}. Nothing was saved"),
    };
    let mut refused = Failure::new(status, failure.code(), "");
    refused.message = message;
    refused
}

fn settings_of(state: &RouterState) -> Result<Arc<ClassifierSettings>, Failure> {
    state.settings.get().cloned().ok_or_else(|| {
        Failure::new(
            StatusCode::NOT_FOUND,
            "settings_unavailable",
            "this router was not started from a configuration file, so it has no settings to save",
        )
    })
}

fn admitted(settings: &ClassifierSettings, headers: &HeaderMap, body: bool) -> Option<Response> {
    let refusal = match &settings.admin {
        Ok(access) => access.check(headers, body).err()?,
        Err(reason) => {
            return Some(
                Failure::new(StatusCode::FORBIDDEN, "admin_unavailable", reason).into_response(),
            );
        }
    };
    tracing::warn!(
        target: targets::ROUTER,
        action = "classifier_settings_refused",
        code = refusal.code,
        "a settings change was refused"
    );
    Some(Failure::new(refusal.status, refusal.code, refusal.message).into_response())
}

fn if_match(headers: &HeaderMap) -> Result<String, Failure> {
    headers
        .get(header::IF_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().trim_matches('"').to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            Failure::new(
                StatusCode::PRECONDITION_REQUIRED,
                "revision_required",
                "send If-Match with the revision last read",
            )
        })
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work).await.unwrap_or_else(|_| {
        Err(Failure::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            "the settings task failed",
        ))
    })
}

/// `GET /api/router/v1/classifier/settings`.
pub(crate) async fn view(State(state): State<Arc<RouterState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = crate::api::authorize(&state, &headers) {
        return refusal;
    }
    let settings = match settings_of(&state) {
        Ok(settings) => settings,
        Err(failure) => return failure.into_response(),
    };
    match blocking(move || settings.snapshot(&state)).await {
        Ok(view) => no_store(axum::Json(view).into_response()),
        Err(failure) => failure.into_response(),
    }
}

/// `PUT /api/router/v1/classifier/settings`.
pub(crate) async fn save(
    State(state): State<Arc<RouterState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let settings = match settings_of(&state) {
        Ok(settings) => settings,
        Err(failure) => return failure.into_response(),
    };
    if let Some(refusal) = admitted(&settings, &headers, true) {
        return refusal;
    }
    let expected = match if_match(&headers) {
        Ok(expected) => expected,
        Err(failure) => return failure.into_response(),
    };
    if body.len() > MAX_BODY_BYTES {
        return Failure::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "body_too_large",
            "the settings body is too large",
        )
        .into_response();
    }
    // A body that does not parse is described by position only: serde's
    // message can quote a value, and a value here may be a key.
    let mut request: SaveRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return Failure::invalid(&format!(
                "the body is not valid settings JSON (line {}, column {}): expected provider, \
                 jev {{base_url, model, timeout_ms?, min_confidence?}} and api_key?",
                err.line(),
                err.column()
            ))
            .into_response();
        }
    };
    request.api_key = match request.api_key.map(|key| key.trim().to_owned()) {
        Some(key) if key.is_empty() => None,
        Some(key) if key.chars().count() > MAX_KEY_CHARS => {
            return Failure::invalid("the API key is too long").into_response();
        }
        Some(key) if key.chars().any(|c| c.is_whitespace() || c.is_control()) => {
            return Failure::invalid("the API key must be one line with no spaces").into_response();
        }
        other => other,
    };
    if request.provider == ProviderKind::Jev && request.jev.is_none() {
        return Failure::invalid("provider \"jev\" needs its base_url and model").into_response();
    }

    let outcome = {
        let _guard = settings.write.lock().await;
        let worker = Arc::clone(&settings);
        blocking(move || worker.save(&expected, request)).await
    };
    let key_action = match outcome {
        Ok(action) => action,
        Err(failure) => {
            tracing::warn!(
                target: targets::ROUTER,
                action = "classifier_settings_save_failed",
                status = failure.status.as_u16(),
                code = failure.code,
                "classifier settings were not saved"
            );
            return failure.into_response();
        }
    };
    respond_saved(
        state,
        settings,
        json!({"outcome": "saved", "key_action": key_action}),
    )
    .await
}

/// `DELETE /api/router/v1/classifier/key`.
pub(crate) async fn remove_key(
    State(state): State<Arc<RouterState>>,
    headers: HeaderMap,
) -> Response {
    let settings = match settings_of(&state) {
        Ok(settings) => settings,
        Err(failure) => return failure.into_response(),
    };
    if let Some(refusal) = admitted(&settings, &headers, false) {
        return refusal;
    }
    let expected = match if_match(&headers) {
        Ok(expected) => expected,
        Err(failure) => return failure.into_response(),
    };
    let outcome = {
        let _guard = settings.write.lock().await;
        let worker = Arc::clone(&settings);
        blocking(move || worker.remove_key(&expected)).await
    };
    if let Err(failure) = outcome {
        return failure.into_response();
    }
    respond_saved(
        state,
        settings,
        json!({"outcome": "saved", "key_action": "removed"}),
    )
    .await
}

async fn respond_saved(
    state: Arc<RouterState>,
    settings: Arc<ClassifierSettings>,
    extra: Value,
) -> Response {
    match blocking(move || settings.snapshot(&state)).await {
        Ok(mut view) => {
            if let (Some(view), Some(extra)) = (view.as_object_mut(), extra.as_object()) {
                view.extend(extra.clone());
            }
            no_store(axum::Json(view).into_response())
        }
        Err(failure) => failure.into_response(),
    }
}

fn no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}

// --- the file -------------------------------------------------------------------------

/// A JSON value that keeps object keys in the order they were written, so a
/// save changes one section and leaves the operator's layout alone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
enum Ordered {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Ordered>),
    Object(IndexMap<String, Ordered>),
}

/// The `auto_route.classifier` object, which must already exist: its
/// candidate routes are the operator's to choose, never invented here.
fn classifier_section(document: &mut Ordered) -> Result<&mut IndexMap<String, Ordered>, Failure> {
    let missing = || {
        Failure::new(
            StatusCode::CONFLICT,
            "classifier_not_configured",
            "router.json has no auto_route.classifier section; add one with its candidate \
             routes first (the panel's Configuration card writes it)",
        )
    };
    let Ordered::Object(root) = document else {
        return Err(missing());
    };
    let Some(Ordered::Object(auto)) = root.get_mut("auto_route") else {
        return Err(missing());
    };
    match auto.get_mut("classifier") {
        Some(Ordered::Object(section)) => Ok(section),
        _ => Err(missing()),
    }
}

/// `auto_route.classifier` of a file's text, for comparison. Key order does
/// not matter here, so a plain [`Value`] compares what was meant.
fn section_of(text: &str) -> Option<Value> {
    let document: Value = serde_json::from_str(text).ok()?;
    document
        .get("auto_route")?
        .get("classifier")
        .filter(|section| section.is_object())
        .cloned()
}

/// What the panel shows of a section: never more than the file says.
fn summary(section: Option<&Value>) -> Value {
    let Some(section) = section else {
        return json!({"provider": null, "candidates": [], "fallback_route": null, "jev": null});
    };
    let jev = section.get("jev").filter(|block| block.is_object()).map(|block| {
        json!({
            "base_url": block.get("base_url").and_then(Value::as_str).unwrap_or(DEFAULT_BASE_URL),
            "model": block.get("model").and_then(Value::as_str),
            "api_key_env": block.get("api_key_env").and_then(Value::as_str)
                .unwrap_or(DEFAULT_API_KEY_ENV),
            "timeout_ms": block.get("timeout_ms").and_then(Value::as_u64),
            "min_confidence": block.get("min_confidence").and_then(Value::as_f64),
        })
    });
    json!({
        "provider": section.get("provider").and_then(Value::as_str).unwrap_or("lightweight"),
        "candidates": section.get("routes").cloned().unwrap_or_else(|| json!([])),
        "fallback_route": section.get("fallback_route"),
        "jev": jev,
    })
}

/// The revision a save must name: the file's SHA-256, so any change — the
/// panel's or an editor's — is seen.
pub fn revision(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Where the file as it was before the last save is kept.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(std::ffi::OsStr::to_os_string)
        .unwrap_or_else(|| "router.json".into());
    name.push(".bak");
    path.with_file_name(name)
}

/// Replace `path` with `bytes` so a crash leaves the old file or the new one,
/// never half of either: a sibling temp file, flushed, then renamed over it.
/// The file keeps its permissions; a new one is owner-only.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("no file name"))?
        .to_string_lossy()
        .into_owned();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let temporary = path.with_file_name(format!(".{name}.{}.{nonce}.tmp", std::process::id()));
    let existing = std::fs::metadata(path).ok().map(|meta| meta.permissions());

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temporary)?;
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        drop(file);
        if let Some(permissions) = existing {
            std::fs::set_permissions(&temporary, permissions)?;
        }
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewrite_keeps_every_key_where_it_was() {
        let text = r#"{"zeta": 1, "listen": ["127.0.0.1:1"], "auto_route": {"rules": [], "classifier": {"routes": ["B", "A"], "provider": "lightweight", "jev": {"model": "x", "include_user_text": false}}}, "alpha": {"y": 2.5, "b": null}}"#;
        let mut document: Ordered = serde_json::from_str(text).unwrap();
        classifier_section(&mut document)
            .unwrap()
            .insert("provider".into(), Ordered::String("jev".into()));
        let written = serde_json::to_string(&document).unwrap();
        assert_eq!(
            written,
            text.replace(' ', "").replace("\"lightweight\"", "\"jev\"")
        );
    }

    #[test]
    fn a_file_without_a_classifier_section_is_not_given_one() {
        let mut document: Ordered = serde_json::from_str(r#"{"auto_route": {}}"#).unwrap();
        assert_eq!(
            classifier_section(&mut document).unwrap_err().code,
            "classifier_not_configured"
        );
    }

    #[test]
    fn the_revision_is_the_content_hash() {
        assert_eq!(revision("a"), revision("a"));
        assert_ne!(revision("a"), revision("a "));
        assert_eq!(revision("").len(), 64);
    }

    #[test]
    fn an_atomic_write_replaces_the_file_and_leaves_no_temporary() {
        let dir = std::env::temp_dir().join(format!("lw-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("router.json");
        std::fs::write(&path, "old").unwrap();
        write_atomically(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
