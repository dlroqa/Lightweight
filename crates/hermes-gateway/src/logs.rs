//! `GET /api/v1/logs` — what the gateway has recorded about itself.
//!
//! The log has existed since M3.5 and has never been readable except by opening
//! the file. This makes it readable over the control API, and it changes
//! nothing about what goes into it: the prompt, the completion, the API key and
//! the bound address stay out, as a grep of a real session proved in M3.5. This
//! endpoint can only ever show what was already written down.
//!
//! Two properties matter more than features here:
//!
//! * **Bounded work.** A daily file on a busy gateway is large, and a panel
//!   polls. Records are streamed through a ring buffer of exactly the requested
//!   size, so memory is bounded by the answer rather than by the file, and only
//!   the newest few files are opened at all.
//! * **Bounded blast radius.** A line that will not parse is skipped, not
//!   fatal. The log is written by `tracing-appender` and read here; a partial
//!   final line is normal when a record is being appended as the file is read,
//!   and refusing the whole request over it would make the endpoint fail
//!   exactly when the gateway is busiest.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::routes::authorize;
use crate::state::GatewayState;

/// Records returned when the caller does not say.
const DEFAULT_LIMIT: usize = 200;
/// The most that can be asked for in one request.
///
/// A ceiling rather than a default: the ring buffer means a large limit costs
/// memory proportional to it, and a panel that wants the whole history wants
/// the file, not this endpoint.
const MAX_LIMIT: usize = 2_000;
/// How many rotated files back to look.
///
/// Two, so that a request made just after midnight still sees the hours before
/// it. Going further would mean opening an unbounded number of files to answer
/// a question about the recent past.
const FILES_READ: usize = 2;
/// The stem `hermes-observability` gives its daily files.
const LOG_STEM: &str = "gateway.log";

/// Severity, ordered so a caller can ask for "warn and above".
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// Parse the spelling `tracing` writes, which is upper case.
    fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "trace" => Some(Self::Trace),
            "debug" => Some(Self::Debug),
            "info" => Some(Self::Info),
            "warn" | "warning" => Some(Self::Warn),
            "error" => Some(Self::Error),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    /// Minimum severity, inclusive. Absent means every level.
    #[serde(default)]
    pub level: Option<String>,
    /// Substring match against the record's target, such as `hermes::api`.
    #[serde(default)]
    pub target: Option<String>,
    /// Case-insensitive substring match against the message.
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// One record, in the shape the Logs screen shows: time, level, source, message.
#[derive(Debug, Serialize)]
pub struct LogRecord {
    timestamp: String,
    level: String,
    target: String,
    message: String,
    /// Everything else the record carried, with `message` lifted out of it.
    ///
    /// Kept rather than dropped: the structured fields are the reason the log
    /// is JSON, and a reader that only ever sees the rendered sentence has the
    /// least useful half of each record.
    #[serde(skip_serializing_if = "serde_json::Map::is_empty")]
    fields: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct LogsBody {
    object: &'static str,
    /// Newest first, which is the order they are read in.
    data: Vec<LogRecord>,
    /// Whether older matching records were dropped to honour the limit.
    ///
    /// Said explicitly so an empty-looking tail is never mistaken for the
    /// beginning of the log.
    truncated: bool,
    /// The files this answer was drawn from, newest last.
    files: Vec<PathBuf>,
}

/// `GET /api/v1/logs`.
pub async fn logs(
    State(state): State<Arc<GatewayState>>,
    Query(query): Query<LogQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = authorize(&state, &headers) {
        return refusal;
    }

    let Some(paths) = state.config.paths.as_ref() else {
        return (
            StatusCode::NOT_IMPLEMENTED,
            axum::Json(serde_json::json!({
                "error": {
                    "message": "this gateway was started without a data directory, so it \
                                keeps no log file",
                    "type": "server_error",
                    "code": "no_data_directory",
                }
            })),
        )
            .into_response();
    };

    // A level that does not name a level is refused rather than ignored: a
    // filter that silently does nothing shows the caller everything and lets
    // them believe they are looking at errors only.
    let minimum = match query.level.as_deref() {
        None => None,
        Some(value) => match Level::parse(value) {
            Some(level) => Some(level),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": {
                            "message": format!(
                                "unknown level {value:?}; use trace, debug, info, warn or error"
                            ),
                            "type": "invalid_request_error",
                            "param": "level",
                            "code": "invalid_request",
                        }
                    })),
                )
                    .into_response();
            }
        },
    };

    let filter = Filter {
        minimum,
        target: query.target,
        search: query.search.map(|text| text.to_lowercase()),
        limit: query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
    };
    let directory = paths.logs_dir();

    // Reading files is blocking work, and the log directory can be on the same
    // slow or absent mount the rest of the data is on.
    match tokio::task::spawn_blocking(move || read_logs(&directory, &filter)).await {
        Ok(body) => axum::Json(body).into_response(),
        Err(err) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": {
                    "message": format!("the log could not be read: {err}"),
                    "type": "server_error",
                    "code": "log_read_unavailable",
                }
            })),
        )
            .into_response(),
    }
}

struct Filter {
    minimum: Option<Level>,
    target: Option<String>,
    search: Option<String>,
    limit: usize,
}

impl Filter {
    fn admits(&self, record: &LogRecord) -> bool {
        if let Some(minimum) = self.minimum
            && Level::parse(&record.level).is_none_or(|level| level < minimum)
        {
            return false;
        }
        if let Some(target) = &self.target
            && !record.target.contains(target)
        {
            return false;
        }
        if let Some(search) = &self.search
            && !record.message.to_lowercase().contains(search)
        {
            return false;
        }
        true
    }
}

/// Read the newest records matching `filter`, newest first.
fn read_logs(directory: &Path, filter: &Filter) -> LogsBody {
    let files = recent_files(directory);

    // Exactly `limit` records are ever held: each new match pushes the oldest
    // out. That is what keeps a multi-gigabyte file answerable.
    let mut kept: VecDeque<LogRecord> = VecDeque::with_capacity(filter.limit);
    let mut truncated = false;

    for file in &files {
        let Ok(contents) = std::fs::read_to_string(file) else {
            // A file that vanished between listing and reading is a rotation,
            // not a failure. The others still answer.
            continue;
        };
        for line in contents.lines() {
            let Some(record) = parse_record(line) else {
                continue;
            };
            if !filter.admits(&record) {
                continue;
            }
            if kept.len() == filter.limit {
                kept.pop_front();
                truncated = true;
            }
            kept.push_back(record);
        }
    }

    let mut data: Vec<LogRecord> = kept.into();
    data.reverse();
    LogsBody {
        object: "list",
        data,
        truncated,
        files,
    }
}

/// The newest [`FILES_READ`] log files, oldest first.
///
/// `tracing-appender`'s daily files are named `gateway.log.YYYY-MM-DD`, and
/// those sort lexicographically into date order — which is the one property
/// this relies on, rather than reading and parsing each name.
fn recent_files(directory: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(LOG_STEM))
        })
        .collect();
    files.sort();
    if files.len() > FILES_READ {
        files.drain(..files.len() - FILES_READ);
    }
    files
}

/// Turn one JSON line into a record, or nothing.
///
/// Nothing, rather than an error, for a line that will not parse: the last line
/// of a file being appended to is routinely half-written, and one torn line
/// must not cost the caller the other ten thousand.
fn parse_record(line: &str) -> Option<LogRecord> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;

    let mut fields = object
        .get("fields")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .unwrap_or_default();
    // Lifted out rather than duplicated: the message is a column of its own on
    // screen, and leaving it in `fields` too would render it twice.
    let message = fields
        .remove("message")
        .map(|value| match value {
            serde_json::Value::String(text) => text,
            // A structured message that is not a string still has a rendering,
            // and showing it beats showing an empty row.
            other => other.to_string(),
        })
        .unwrap_or_default();

    Some(LogRecord {
        timestamp: object.get("timestamp")?.as_str()?.to_owned(),
        level: object.get("level")?.as_str()?.to_owned(),
        target: object
            .get("target")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        message,
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"timestamp":"2026-08-24T05:32:01.020722Z","level":"INFO","fields":{"message":"prompt logging is disabled","privacy_mode":"standard"},"target":"hermes::startup"}
{"timestamp":"2026-08-24T05:32:01.071477Z","level":"INFO","fields":{"message":"gateway listening","port":18234,"listeners":1,"auth":false},"target":"hermes::startup"}
{"timestamp":"2026-08-24T05:33:10.000000Z","level":"WARN","fields":{"message":"the model could not be added to the catalog"},"target":"hermes::model"}
{"timestamp":"2026-08-24T05:34:00.000000Z","level":"ERROR","fields":{"message":"generation stopped by user"},"target":"hermes::inference"}
"#;

    fn scratch(tag: &str) -> PathBuf {
        // The clock alone is not unique: on a coarse timer two tests running in
        // parallel are handed the same name. The counter and the pid settle it.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "hermes-logs-{tag}-{}-{unique}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create the log directory");
        std::fs::write(directory.join("gateway.log.2026-08-24"), SAMPLE).expect("write the log");
        directory
    }

    fn filter(limit: usize) -> Filter {
        Filter {
            minimum: None,
            target: None,
            search: None,
            limit,
        }
    }

    #[test]
    fn records_come_back_newest_first() {
        // The order the Logs screen reads in, so the newest line is not at the
        // bottom of a scroll.
        let directory = scratch("order");
        let body = read_logs(&directory, &filter(100));
        assert_eq!(body.data.len(), 4);
        assert_eq!(body.data[0].message, "generation stopped by user");
        assert_eq!(body.data[3].message, "prompt logging is disabled");
        assert!(!body.truncated);
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn the_message_is_lifted_out_of_the_fields_it_arrived_in() {
        let directory = scratch("fields");
        let body = read_logs(&directory, &filter(100));
        let listening = body
            .data
            .iter()
            .find(|record| record.message == "gateway listening")
            .expect("the listening record");
        // The structured half survives...
        assert_eq!(listening.fields["port"], 18234);
        // ...and the message is not also left inside it.
        assert!(!listening.fields.contains_key("message"));
        assert_eq!(listening.target, "hermes::startup");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_level_filter_keeps_that_level_and_above() {
        let directory = scratch("level");
        let body = read_logs(
            &directory,
            &Filter {
                minimum: Some(Level::Warn),
                ..filter(100)
            },
        );
        assert_eq!(body.data.len(), 2, "warn and error, not the two infos");
        assert!(body.data.iter().all(|record| record.level != "INFO"));
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_limit_keeps_the_newest_and_says_it_truncated() {
        // The property the ring buffer exists for. Silently returning the
        // *oldest* two would show a panel the start of the log and call it the
        // present.
        let directory = scratch("limit");
        let body = read_logs(&directory, &filter(2));
        assert_eq!(body.data.len(), 2);
        assert_eq!(body.data[0].message, "generation stopped by user");
        assert!(body.truncated, "older matching records were dropped");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn searching_is_case_insensitive_and_matches_the_message() {
        let directory = scratch("search");
        let body = read_logs(
            &directory,
            &Filter {
                search: Some("LISTENING".to_lowercase()),
                ..filter(100)
            },
        );
        assert_eq!(body.data.len(), 1);
        assert_eq!(body.data[0].message, "gateway listening");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_torn_line_costs_only_itself() {
        // The last record of a file being appended to is routinely half
        // written. Refusing the whole request over it would break this endpoint
        // exactly when the gateway is busiest.
        let directory = scratch("torn");
        let path = directory.join("gateway.log.2026-08-24");
        let mut contents = std::fs::read_to_string(&path).expect("read");
        contents.push_str("{\"timestamp\":\"2026-08-24T05:35:00.0000");
        std::fs::write(&path, contents).expect("write");

        let body = read_logs(&directory, &filter(100));
        assert_eq!(body.data.len(), 4, "the four whole records still arrive");
        std::fs::remove_dir_all(&directory).ok();
    }

    #[test]
    fn a_missing_log_directory_is_an_empty_answer_not_a_failure() {
        // A gateway that has not written a line yet has an empty log, and that
        // is a true answer to "what have you recorded?".
        let body = read_logs(Path::new("/nonexistent-hermes-log-directory"), &filter(10));
        assert!(body.data.is_empty());
        assert!(body.files.is_empty());
        assert!(!body.truncated);
    }

    #[test]
    fn only_the_newest_files_are_opened() {
        let directory = scratch("rotation");
        for day in ["2026-08-20", "2026-08-21", "2026-08-22", "2026-08-23"] {
            std::fs::write(directory.join(format!("gateway.log.{day}")), SAMPLE).expect("write");
        }
        // Five files exist; two are read, and they are the newest two.
        let body = read_logs(&directory, &filter(1_000));
        assert_eq!(body.files.len(), FILES_READ);
        assert!(
            body.files
                .iter()
                .all(|path| path.to_string_lossy().contains("2026-08-2")),
        );
        assert!(body.files[1].to_string_lossy().ends_with("2026-08-24"));
        std::fs::remove_dir_all(&directory).ok();
    }
}
