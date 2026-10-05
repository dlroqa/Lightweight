//! Errors the router answers with itself.
//!
//! The same envelope the gateway uses — `{"error":{message,type,param,code}}`
//! — so a client handles a refusal from the router exactly as it handles one
//! from a node. Errors that come *from* a node are forwarded as the node wrote
//! them, not re-wrapped; this module is only for what the router decides on its
//! own.

use std::time::Duration;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use lightweight_api::error::{ErrorBody, ErrorEnvelope};
use lightweight_gateway::auth::AuthFailure;

use crate::domain::RoutingFailure;

/// Send `envelope` with `status`.
pub fn json_error(status: StatusCode, envelope: &ErrorEnvelope) -> Response {
    let body = serde_json::to_vec(envelope).unwrap_or_else(|_| {
        br#"{"error":{"message":"internal error","type":"server_error","code":"internal"}}"#
            .to_vec()
    });
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// A server-side error envelope, which the gateway's API types build only from
/// a workspace error.
pub fn server_error(message: impl Into<String>, code: impl Into<String>) -> ErrorEnvelope {
    ErrorEnvelope {
        error: ErrorBody {
            message: message.into(),
            r#type: "server_error".to_owned(),
            param: None,
            code: code.into(),
            hermes: None,
        },
    }
}

/// The answer to a request that could not be routed.
///
/// * An unknown route is the gateway's own `model_not_found`, with the same
///   status and `param`, so a client that already handles a node refusing a
///   model handles the router refusing a route.
/// * No default route is a client error, and says what to do instead.
/// * A known route with nothing available is `route_unavailable`, a 503 with
///   `Retry-After` set to the probe interval — the soonest the answer can
///   change.
pub fn routing_failure(failure: &RoutingFailure, retry_after: Duration) -> Response {
    match failure {
        RoutingFailure::UnknownRoute { requested } => json_error(
            StatusCode::NOT_FOUND,
            &ErrorEnvelope::invalid_request(
                format!(
                    "the model {requested:?} is not served by this router; GET /v1/models lists the models it serves"
                ),
                "model_not_found",
            )
            .with_param("model"),
        ),
        RoutingFailure::NoDefaultRoute => json_error(
            StatusCode::BAD_REQUEST,
            &ErrorEnvelope::invalid_request(
                "this router has no default route configured, so `default` or an omitted model \
                 cannot be resolved; name a model from GET /v1/models",
                "no_default_route",
            )
            .with_param("model"),
        ),
        RoutingFailure::RouteUnavailable { route } => {
            let mut response = json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                &server_error(
                    format!("No healthy deployment is available for route {:?}.", route.as_str()),
                    "route_unavailable",
                ),
            );
            if let Ok(value) = HeaderValue::from_str(&retry_after.as_secs().max(1).to_string()) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
            response
        }
    }
}

/// The 401 a refused credential becomes — the gateway's body, word for word.
pub fn unauthorized(failure: AuthFailure) -> Response {
    let code = match failure {
        AuthFailure::Missing => "missing_api_key",
        AuthFailure::Invalid => "invalid_api_key",
    };
    json_error(
        StatusCode::UNAUTHORIZED,
        &ErrorEnvelope {
            error: ErrorBody {
                message: failure.message().to_owned(),
                r#type: "authentication_error".to_owned(),
                param: None,
                code: code.to_owned(),
                hermes: None,
            },
        },
    )
}
