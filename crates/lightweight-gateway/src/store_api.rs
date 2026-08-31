//! `/api/v1/conversations` and `/api/v1/settings`.
//!
//! The panel owns the conversation while it is being had; this is where it puts
//! it so that closing the window is not the same as losing it. Writes replace
//! the whole conversation rather than appending a turn, which keeps the wire
//! contract to one verb and means there is no partial-append state to recover
//! from — the file is either the conversation before this turn or after it.
//!
//! Every path here does file I/O, so every path runs under `spawn_blocking`.
//! The data directory can be a network mount, and an endpoint that blocks a
//! runtime worker on it would take the gateway down with the mount.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use lightweight_core::Actionable;
use lightweight_store::{
    ApiConfig, ApiKeyRecord, Conversation, ConversationStore, RateLimit, Settings, StoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::routes::authorize;
use crate::scheduler::PeerKey;
use crate::state::GatewayState;

/// `GET /api/v1/conversations`.
pub async fn list(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.conversations() else {
        return no_data_directory();
    };
    blocking(move || store.list())
        .await
        .map_or_else(Failure::into_response, |rows| {
            axum::Json(json!({ "object": "list", "data": rows })).into_response()
        })
}

/// `POST /api/v1/conversations` — start one, and get the only id it will have.
///
/// The id is generated here and never taken from the request. It becomes a file
/// name, and accepting one from a caller means accepting a path from a caller.
pub async fn create(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.conversations() else {
        return no_data_directory();
    };
    if let Some(refusal) = refuse_if_history_is_off(&state) {
        return refusal;
    }

    let now = unix_now();
    let conversation = Conversation {
        id: ConversationStore::new_id(),
        title: String::new(),
        created_at: now,
        updated_at: now,
        model: None,
        messages: Vec::new(),
    };

    let saved = conversation.clone();
    match blocking(move || store.save(&saved)).await {
        Ok(()) => (StatusCode::CREATED, axum::Json(conversation)).into_response(),
        Err(failure) => failure.into_response(),
    }
}

/// `GET /api/v1/conversations/{id}`.
pub async fn get(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.conversations() else {
        return no_data_directory();
    };
    blocking(move || store.get(&id))
        .await
        .map_or_else(Failure::into_response, |conversation| {
            axum::Json(conversation).into_response()
        })
}

/// What a client may send when saving a conversation.
///
/// The id is taken from the path, never from the body: a body that disagreed
/// with the path would be two answers to "which conversation is this?".
#[derive(Debug, Deserialize)]
pub struct SaveConversation {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub messages: Vec<lightweight_store::StoredMessage>,
    /// Preserved from the existing conversation when absent, so a client that
    /// does not track it cannot reset when a conversation began.
    #[serde(default)]
    pub created_at: Option<u64>,
}

/// `PUT /api/v1/conversations/{id}`.
pub async fn save(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.conversations() else {
        return no_data_directory();
    };
    if let Some(refusal) = refuse_if_history_is_off(&state) {
        return refusal;
    }

    let incoming: SaveConversation = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(err) => return bad_request("body", &err.to_string()),
    };

    let result = blocking(move || {
        // `created_at` comes from the client, then from what is already on
        // disk, then from now. A conversation's beginning is a fact about it
        // that a later save must not be able to rewrite by omission.
        let existing_created_at = store.get(&id).ok().map(|existing| existing.created_at);
        let now = unix_now();
        let conversation = Conversation {
            created_at: incoming.created_at.or(existing_created_at).unwrap_or(now),
            id,
            title: incoming.title,
            updated_at: now,
            model: incoming.model,
            messages: incoming.messages,
        };
        store.save(&conversation).map(|()| conversation)
    })
    .await;

    result.map_or_else(Failure::into_response, |conversation| {
        axum::Json(conversation).into_response()
    })
}

/// `DELETE /api/v1/conversations/{id}`.
pub async fn delete(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.conversations() else {
        return no_data_directory();
    };
    let deleted = id.clone();
    blocking(move || store.delete(&deleted))
        .await
        .map_or_else(Failure::into_response, |()| {
            axum::Json(json!({ "deleted": id })).into_response()
        })
}

/// `GET /api/v1/settings`.
pub async fn settings(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.settings_store() else {
        return no_data_directory();
    };
    blocking(move || store.load())
        .await
        .map_or_else(Failure::into_response, |settings| {
            axum::Json(settings).into_response()
        })
}

/// `PUT /api/v1/settings`.
///
/// The whole document, not a patch. A patch would need a merge rule for the
/// opaque `ui` half, and any rule chosen here would be this crate having an
/// opinion about preferences it deliberately does not understand.
pub async fn save_settings(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.settings_store() else {
        return no_data_directory();
    };

    let incoming: Settings = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(err) => return bad_request("body", &err.to_string()),
    };

    blocking(move || store.save(&incoming).map(|()| incoming))
        .await
        .map_or_else(Failure::into_response, |settings| {
            axum::Json(settings).into_response()
        })
}

/// Read the settings the gateway itself acts on.
///
/// Defaults when there is no file, and — deliberately — defaults when the file
/// cannot be read. This is consulted on the load path, and a corrupt settings
/// file must not be able to stop a model from loading; the endpoint that exists
/// to show settings reports the corruption plainly.
pub fn gateway_settings(state: &GatewayState) -> lightweight_store::GatewaySettings {
    state
        .settings_store()
        .and_then(|store| store.load().ok())
        .unwrap_or_default()
        .gateway
}

/// Refuse a write when the user has turned history off.
///
/// Reads are still allowed: conversations saved before the setting changed are
/// still theirs, and hiding them would leave no way to look at or delete them.
fn refuse_if_history_is_off(state: &GatewayState) -> Option<Response> {
    if gateway_settings(state).keep_history {
        return None;
    }
    Some(store_error(&StoreError::HistoryDisabled))
}

/// Why a store operation did not produce a value.
///
/// Kept as a small value rather than as a built `Response`: an `axum` response
/// is over a hundred bytes, and putting one in every `Result` here would make
/// the success path carry the cost of the failure path.
enum Failure {
    Store(StoreError),
    /// The runtime is shutting down, so the work never ran.
    Unreachable(String),
}

impl Failure {
    fn into_response(self) -> Response {
        match self {
            Self::Store(err) => store_error(&err),
            Self::Unreachable(reason) => (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({
                    "error": {
                        "message": format!("the store could not be reached: {reason}"),
                        "type": "server_error",
                        "code": "store_unavailable",
                    }
                })),
            )
                .into_response(),
        }
    }
}

/// Run blocking store work off the runtime.
async fn blocking<T, F>(work: F) -> Result<T, Failure>
where
    F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(Failure::Store(err)),
        Err(err) => Err(Failure::Unreachable(err.to_string())),
    }
}

fn store_error(err: &StoreError) -> Response {
    let status =
        StatusCode::from_u16(err.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        axum::Json(lightweight_api::error::ErrorEnvelope::from_error(err)),
    )
        .into_response()
}

fn no_data_directory() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        axum::Json(json!({
            "error": {
                "message": "this gateway was started without a data directory, so it \
                            keeps no conversations or settings",
                "type": "server_error",
                "code": "no_data_directory",
            }
        })),
    )
        .into_response()
}

fn bad_request(param: &str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "param": param,
                "code": "invalid_request",
            }
        })),
    )
        .into_response()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or_default()
}

// --- API keys and bind configuration -----------------------------------------
//
// These are the security-relevant control endpoints. Two rules run through all
// of them:
//
//   * **Reads are open to any authenticated caller; writes are loopback-only.**
//     A key handed to a remote agent lets it *use* the gateway. It must not let
//     it mint more keys or widen the bind set — revoking a leaked key is
//     pointless if its holder can add three more, and broadening exposure
//     should take physical access to the machine. `PeerKey::is_local` draws
//     that line without the endpoint ever seeing the address.
//   * **No secret is ever returned, with exactly one exception:** the plaintext
//     of a key at the moment it is created, and only over a loopback request.
//     Every list view is the redacted [`KeyView`], which carries a prefix and a
//     hash-free record — never the key, never even its hash.

/// A key as the panel sees it: enough to name, meter and revoke, nothing that
/// could be presented as a credential.
#[derive(Serialize)]
struct KeyView {
    id: String,
    name: String,
    prefix: String,
    created_at: u64,
    limit: RateLimit,
    total: u64,
    last_used: Option<u64>,
    in_last_minute: u32,
    today: u32,
}

impl KeyView {
    fn of(record: ApiKeyRecord, usage: crate::limits::UsageSnapshot) -> Self {
        Self {
            id: record.id,
            name: record.name,
            prefix: record.prefix,
            created_at: record.created_at,
            limit: record.limit,
            total: usage.total,
            last_used: usage.last_used,
            in_last_minute: usage.in_last_minute,
            today: usage.today,
        }
    }
}

/// Refuse a write that did not come from this machine.
fn require_local(peer: &PeerKey) -> Option<Response> {
    if peer.is_local() {
        return None;
    }
    Some(
        (
            StatusCode::FORBIDDEN,
            axum::Json(json!({
                "error": {
                    "message": "changing keys or the bind configuration is only allowed \
                                from the machine running the gateway",
                    "type": "invalid_request_error",
                    "code": "local_only",
                }
            })),
        )
            .into_response(),
    )
}

/// `GET /api/v1/gateway/keys` — every key, redacted, with its live usage.
pub async fn list_keys(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.api_keys_store() else {
        return no_data_directory();
    };
    match blocking(move || store.list()).await {
        Ok(records) => {
            let views: Vec<KeyView> = records
                .into_iter()
                .map(|record| {
                    let usage = state.limiter().snapshot(&record.id);
                    KeyView::of(record, usage)
                })
                .collect();
            axum::Json(json!({ "object": "list", "data": views })).into_response()
        }
        Err(failure) => failure.into_response(),
    }
}

/// The body of a create request.
#[derive(Deserialize, Default)]
#[serde(default)]
struct CreateKey {
    name: String,
    limit: RateLimit,
}

/// `POST /api/v1/gateway/keys` — mint a key, returning its plaintext once.
pub async fn create_key(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    peer: PeerKey,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    if let Some(refusal) = require_local(&peer) {
        return refusal;
    }
    let Some(store) = state.api_keys_store() else {
        return no_data_directory();
    };
    let request: CreateKey = if body.is_empty() {
        CreateKey::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(parsed) => parsed,
            Err(err) => return bad_request("body", &err.to_string()),
        }
    };

    match blocking(move || store.create(&request.name, request.limit)).await {
        Ok((record, plaintext)) => (
            StatusCode::CREATED,
            // The one place a plaintext key leaves the gateway, and only ever to
            // a loopback caller: this is the moment it is shown, once.
            axum::Json(json!({
                "id": record.id,
                "name": record.name,
                "prefix": record.prefix,
                "created_at": record.created_at,
                "limit": record.limit,
                "key": plaintext,
            })),
        )
            .into_response(),
        Err(failure) => failure.into_response(),
    }
}

/// `DELETE /api/v1/gateway/keys/{id}` — revoke a key.
pub async fn revoke_key(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    peer: PeerKey,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    if let Some(refusal) = require_local(&peer) {
        return refusal;
    }
    let Some(store) = state.api_keys_store() else {
        return no_data_directory();
    };
    let for_store = id.clone();
    match blocking(move || store.revoke(&for_store)).await {
        Ok(removed) => {
            if removed {
                // Free the live counters so a future id cannot inherit them.
                state.limiter().forget(&id);
                (
                    StatusCode::OK,
                    axum::Json(json!({ "revoked": true, "id": id })),
                )
                    .into_response()
            } else {
                (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({
                        "error": {
                            "message": "no key with that id",
                            "type": "invalid_request_error",
                            "code": "not_found",
                        }
                    })),
                )
                    .into_response()
            }
        }
        Err(failure) => failure.into_response(),
    }
}

/// `PUT /api/v1/gateway/keys/{id}/limit` — change one key's ceiling.
pub async fn set_key_limit(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    peer: PeerKey,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    if let Some(refusal) = require_local(&peer) {
        return refusal;
    }
    let Some(store) = state.api_keys_store() else {
        return no_data_directory();
    };
    let limit: RateLimit = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(err) => return bad_request("body", &err.to_string()),
    };
    match blocking(move || store.set_limit(&id, limit)).await {
        Ok(true) => (StatusCode::OK, axum::Json(json!({ "updated": true }))).into_response(),
        Ok(false) => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "error": {
                    "message": "no key with that id",
                    "type": "invalid_request_error",
                    "code": "not_found",
                }
            })),
        )
            .into_response(),
        Err(failure) => failure.into_response(),
    }
}

/// `GET /api/v1/gateway/config` — the persisted bind configuration.
pub async fn get_config(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let Some(store) = state.api_config_store() else {
        return no_data_directory();
    };
    let running: Vec<std::net::SocketAddr> = state.config.bound_addresses.clone();
    match blocking(move || store.load()).await {
        Ok(config) => {
            let matches_running = config_matches_running(&config, &running);
            axum::Json(json!({
                "hosts": config.hosts,
                "port": config.port,
                "matches_running": matches_running,
            }))
            .into_response()
        }
        Err(failure) => failure.into_response(),
    }
}

/// `PUT /api/v1/gateway/config` — replace the bind configuration.
pub async fn save_config(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    peer: PeerKey,
    body: axum::body::Bytes,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    if let Some(refusal) = require_local(&peer) {
        return refusal;
    }
    let Some(store) = state.api_config_store() else {
        return no_data_directory();
    };
    let incoming: ApiConfig = match serde_json::from_slice(&body) {
        Ok(parsed) => parsed,
        Err(err) => return bad_request("body", &err.to_string()),
    };

    let port = incoming.port.unwrap_or(crate::DEFAULT_PORT);
    let has_credential = state.config.auth.is_enabled()
        || state
            .api_keys_store()
            .and_then(|keys| keys.any().ok())
            .unwrap_or(false);
    let hosts = incoming.hosts.clone();

    // Resolution is synchronous DNS, so it rides the blocking pool with the
    // save it gates.
    match blocking(move || {
        Ok(validate_and_save(
            &store,
            &incoming,
            port,
            has_credential,
            &hosts,
        ))
    })
    .await
    {
        Ok(Ok(saved)) => axum::Json(json!({
            "hosts": saved.hosts,
            "port": saved.port,
            "restart_required": true,
        }))
        .into_response(),
        Ok(Err((param, message))) => bad_request(param, &message),
        Err(failure) => failure.into_response(),
    }
}

/// Validate a bind configuration and, if it is sound, persist it.
///
/// The checks exist to make it impossible to save a configuration that would
/// refuse to start or lock the panel out of its own gateway:
///
///   * every host must resolve — a typo becomes a message now, not a failed
///     start later;
///   * at least one resolved address must be loopback, so the machine running
///     the gateway (and the desktop window, which loads `127.0.0.1`) can always
///     reach it;
///   * an exposed bind with no key configured is refused, the same rule the CLI
///     enforces at startup.
fn validate_and_save(
    store: &lightweight_store::ApiConfigStore,
    config: &ApiConfig,
    port: u16,
    has_credential: bool,
    hosts: &[String],
) -> Result<ApiConfig, (&'static str, String)> {
    use std::net::ToSocketAddrs;

    // An empty host list means "no opinion": the gateway falls back to loopback,
    // which is always safe. Nothing to validate.
    if !hosts.is_empty() {
        let mut resolved = Vec::new();
        for host in hosts {
            let addrs = (host.as_str(), port)
                .to_socket_addrs()
                .map_err(|err| ("hosts", format!("'{host}' could not be resolved: {err}")))?;
            resolved.extend(addrs.map(|addr| addr.ip()));
        }
        if resolved.is_empty() {
            return Err((
                "hosts",
                "none of the hosts resolved to an address".to_owned(),
            ));
        }
        if !resolved.iter().any(std::net::IpAddr::is_loopback) {
            return Err((
                "hosts",
                "at least one host must be loopback (127.0.0.1 or localhost), so this \
                 machine can always reach the gateway"
                    .to_owned(),
            ));
        }
        let exposed = resolved.iter().any(|ip| !ip.is_loopback());
        if exposed && !has_credential {
            return Err((
                "hosts",
                "binding to an address other machines can reach needs an API key; \
                 create one first with `hermes key create`"
                    .to_owned(),
            ));
        }
    }

    store
        .save(config)
        .map_err(|err| ("body", format!("could not save the configuration: {err}")))?;
    Ok(config.clone())
}

/// Whether the persisted config, once resolved, matches what is bound now.
fn config_matches_running(config: &ApiConfig, running: &[std::net::SocketAddr]) -> bool {
    use std::collections::BTreeSet;
    use std::net::ToSocketAddrs;

    if config.hosts.is_empty() {
        return true; // No opinion recorded; nothing to disagree with.
    }
    let port = config.port.unwrap_or(crate::DEFAULT_PORT);
    let mut want = BTreeSet::new();
    for host in &config.hosts {
        match (host.as_str(), port).to_socket_addrs() {
            Ok(addrs) => want.extend(addrs),
            Err(_) => return false,
        }
    }
    let have: BTreeSet<_> = running.iter().copied().collect();
    want == have
}
