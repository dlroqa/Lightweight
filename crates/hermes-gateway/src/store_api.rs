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
use hermes_core::Actionable;
use hermes_store::{Conversation, ConversationStore, Settings, StoreError};
use serde::Deserialize;
use serde_json::json;

use crate::routes::authorize;
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
    pub messages: Vec<hermes_store::StoredMessage>,
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
pub fn gateway_settings(state: &GatewayState) -> hermes_store::GatewaySettings {
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
        axum::Json(hermes_api::error::ErrorEnvelope::from_error(err)),
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
