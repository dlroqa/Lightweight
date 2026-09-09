//! Start the local agent API from the panel. Only a fixed `lightagent serve`
//! command is allowed, and only at a configured loopback HTTP origin.

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use tokio::{io::AsyncReadExt, process::Command, sync::Mutex};

use crate::{GatewayState, routes::authorize};

#[derive(Default)]
pub struct AgentServer {
    inner: Mutex<Managed>,
}

#[derive(Default)]
struct Managed {
    active: bool,
    starting: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct Report {
    status: &'static str,
    upstream: Option<String>,
    can_start: bool,
    message: Option<String>,
}

fn local_address(upstream: Option<&str>) -> Result<(String, u16), String> {
    let upstream = upstream.ok_or("Agent forwarding is disabled. Restart the gateway with --agent-upstream http://127.0.0.1:8735.")?;
    let url =
        reqwest::Url::parse(upstream).map_err(|_| "The agent upstream is not a valid URL.")?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "Start here requires a loopback HTTP origin without credentials, a path, or a query."
                .into(),
        );
    }
    // Match only the loopback addresses supported by `lightagent serve`.
    let host = match url.host_str() {
        Some("127.0.0.1" | "localhost") => "127.0.0.1",
        _ => return Err("This agent is hosted elsewhere. Start lightagent serve on that host, or configure --agent-upstream http://127.0.0.1:8735 to start it here.".into()),
    };
    let port = url
        .port_or_known_default()
        .filter(|port| *port != 0)
        .ok_or("The agent upstream must have a valid port.")?;
    Ok((host.to_owned(), port))
}

fn binary() -> PathBuf {
    select_binary(
        std::env::var_os("LIGHTAGENT_BIN").map(PathBuf::from),
        std::env::current_exe().ok(),
        std::env::var_os("HOME").map(PathBuf::from),
        |path| path.is_file(),
    )
}

fn select_binary(
    explicit: Option<PathBuf>,
    current: Option<PathBuf>,
    home: Option<PathBuf>,
    exists: impl Fn(&std::path::Path) -> bool,
) -> PathBuf {
    // An explicit override is authoritative, including when it is missing:
    // report that path instead of silently launching a different installation.
    if let Some(path) = explicit {
        return path;
    }
    let name = if cfg!(windows) {
        "lightagent.exe"
    } else {
        "lightagent"
    };
    if let Some(current) = current
        && let Some(dir) = current.parent()
    {
        let sibling = dir.join(name);
        if exists(&sibling) {
            return sibling;
        }
    }
    // Desktop launchers often inherit a smaller PATH than terminal shells.
    // This is the documented per-user CLI install location on Linux/macOS.
    if cfg!(unix)
        && let Some(home) = home
    {
        let installed = home.join(".local/bin").join(name);
        if exists(&installed) {
            return installed;
        }
    }
    PathBuf::from(name)
}

async fn healthy(upstream: &str) -> bool {
    let response = crate::agent_proxy::client()
        .get(format!("{}/health", upstream.trim_end_matches('/')))
        .timeout(Duration::from_secs(1))
        .send()
        .await;
    let Ok(response) = response else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    response
        .json::<serde_json::Value>()
        .await
        .is_ok_and(|body| body["service"] == "lightagent" && body["status"] == "ok")
}

async fn report(state: &GatewayState) -> Report {
    let upstream = state.config.agent_upstream.clone();
    let reachable = match &upstream {
        Some(upstream) => healthy(upstream).await,
        None => false,
    };
    let local = local_address(upstream.as_deref());
    let inner = state.agent_server.inner.lock().await;
    Report {
        status: if reachable {
            "running"
        } else if inner.starting {
            "starting"
        } else if inner.active {
            "unavailable"
        } else if inner.error.is_some() {
            "failed"
        } else {
            "stopped"
        },
        can_start: !reachable && !inner.active && local.is_ok(),
        message: if reachable {
            None
        } else {
            inner.error.clone().or_else(|| local.err())
        },
        upstream,
    }
}

pub async fn status(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    Json(report(&state).await).into_response()
}

pub async fn start(State(state): State<Arc<GatewayState>>, headers: HeaderMap) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }
    let (host, port) = match local_address(state.config.agent_upstream.as_deref()) {
        Ok(address) => address,
        Err(message) => return failure(StatusCode::BAD_REQUEST, &message),
    };
    if state.shutdown_token().is_cancelled() {
        return failure(
            StatusCode::SERVICE_UNAVAILABLE,
            "The gateway is shutting down.",
        );
    }
    // Serialize the probe and launch decision: concurrent clicks cannot create
    // two children, and an already running external agent stays external.
    let mut inner = state.agent_server.inner.lock().await;
    let upstream = state.config.agent_upstream.clone().unwrap_or_default();
    if !inner.active && !healthy(&upstream).await {
        let executable = binary();
        let mut command = Command::new(&executable);
        command
            .args(["serve", "--host", &host, "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let message = format!(
                    "Could not start lightagent serve using {}: {error}. Install/build the lightagent binary beside the gateway, in ~/.local/bin, or on PATH; alternatively set LIGHTAGENT_BIN before starting the gateway.",
                    executable.display()
                );
                inner.error = Some(message.clone());
                return failure(StatusCode::SERVICE_UNAVAILABLE, &message);
            }
        };
        inner.active = true;
        inner.starting = true;
        inner.error = None;
        tokio::spawn(supervise(
            Arc::clone(&state.agent_server),
            state.shutdown_token(),
            child,
            upstream,
        ));
    }
    drop(inner);
    (StatusCode::ACCEPTED, Json(report(&state).await)).into_response()
}

fn failure(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": {
            "code": "agent_start_failed", "message": message, "remedies": []
        }})),
    )
        .into_response()
}

async fn supervise(
    server: Arc<AgentServer>,
    shutdown: tokio_util::sync::CancellationToken,
    mut child: tokio::process::Child,
    upstream: String,
) {
    // Drain stderr continuously with bounded storage so a noisy child cannot
    // block on a full pipe or consume unbounded memory.
    let output = Arc::new(Mutex::new(Vec::<u8>::new()));
    let reader = child.stderr.take().map(|mut stderr| {
        let output = Arc::clone(&output);
        tokio::spawn(async move {
            let mut buffer = [0; 1024];
            while let Ok(count) = stderr.read(&mut buffer).await {
                if count == 0 {
                    break;
                }
                let mut tail = output.lock().await;
                tail.extend_from_slice(&buffer[..count]);
                let excess = tail.len().saturating_sub(4096);
                tail.drain(..excess);
            }
        })
    });
    let deadline = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(deadline);
    let mut ready = false;
    let error = loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                let _ = child.kill().await;
                break None;
            }
            result = child.wait() => {
                break Some(format!("lightagent serve exited: {}.", match result {
                    Ok(status) => status.to_string(), Err(error) => error.to_string(),
                }));
            }
            _ = &mut deadline, if !ready => {
                let _ = child.kill().await;
                break Some("The agent did not become ready within 30 seconds. Check its configuration and try again.".into());
            }
            _ = tokio::time::sleep(Duration::from_millis(250)), if !ready => {
                if healthy(&upstream).await {
                    ready = true;
                    server.inner.lock().await.starting = false;
                }
            }
        }
    };
    if let Some(mut reader) = reader {
        // Descendants may inherit stderr; do not wait forever for their EOF.
        if tokio::time::timeout(Duration::from_millis(250), &mut reader)
            .await
            .is_err()
        {
            reader.abort();
        }
    }
    let detail = output.lock().await;
    let mut inner = server.inner.lock().await;
    inner.active = false;
    inner.starting = false;
    inner.error = error.map(|message| {
        let tail = String::from_utf8_lossy(&detail);
        if tail.trim().is_empty() {
            message
        } else {
            format!("{message}\n{}", tail.trim())
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_override_and_bundled_binary_take_priority() {
        let override_path = PathBuf::from("custom-agent");
        assert_eq!(
            select_binary(Some(override_path.clone()), None, None, |_| false),
            override_path
        );
        let name = if cfg!(windows) {
            "lightagent.exe"
        } else {
            "lightagent"
        };
        assert_eq!(
            select_binary(
                None,
                Some(PathBuf::from("bundle/bin/hermes")),
                Some(PathBuf::from("home")),
                |_| true
            ),
            PathBuf::from("bundle/bin").join(name)
        );
    }

    #[cfg(unix)]
    #[test]
    fn desktop_finds_the_user_install_without_a_shell_path() {
        let installed = PathBuf::from("home/.local/bin/lightagent");
        assert_eq!(
            select_binary(
                None,
                Some(PathBuf::from("bundle/bin/hermes")),
                Some(PathBuf::from("home")),
                |path| path == installed
            ),
            installed
        );
        assert_eq!(
            select_binary(None, None, Some(PathBuf::from("home")), |_| false),
            PathBuf::from("lightagent")
        );
    }

    #[test]
    fn only_local_http_origins_can_launch_a_process() {
        for origin in [
            None,
            Some("https://127.0.0.1:8735"),
            Some("http://example.com:8735"),
            Some("http://0.0.0.0:8735"),
            Some("http://127.0.0.1:8735/path"),
            Some("http://user:secret@127.0.0.1:8735"),
            Some("http://127.0.0.1:8735?x=1"),
            Some("http://127.0.0.1:0"),
            Some("garbage"),
        ] {
            assert!(local_address(origin).is_err(), "{origin:?}");
        }
        assert_eq!(
            local_address(Some("http://127.0.0.1:8735/")).unwrap(),
            ("127.0.0.1".into(), 8735)
        );
        assert_eq!(
            local_address(Some("http://localhost:9876")).unwrap(),
            ("127.0.0.1".into(), 9876)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_errors_are_retained_and_allow_retry() {
        let server = Arc::new(AgentServer::default());
        server.inner.lock().await.active = true;
        server.inner.lock().await.starting = true;
        let child = Command::new("/bin/sh")
            .args([
                "-c",
                "echo 'no active profile — run lightagent init first' >&2; exit 1",
            ])
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        supervise(
            Arc::clone(&server),
            tokio_util::sync::CancellationToken::new(),
            child,
            "http://127.0.0.1:1".into(),
        )
        .await;
        let inner = server.inner.lock().await;
        assert!(!inner.active);
        assert!(!inner.starting);
        assert!(inner.error.as_ref().unwrap().contains("lightagent init"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gateway_shutdown_reaps_its_agent_child() {
        let server = Arc::new(AgentServer::default());
        let child = Command::new("/bin/sleep")
            .arg("60")
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        token.cancel();
        tokio::time::timeout(
            Duration::from_secs(2),
            supervise(
                Arc::clone(&server),
                token,
                child,
                "http://127.0.0.1:1".into(),
            ),
        )
        .await
        .unwrap();
        let inner = server.inner.lock().await;
        assert!(!inner.active);
        assert!(inner.error.is_none());
    }
}
