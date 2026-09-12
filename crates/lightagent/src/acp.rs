//! `lightagent acp` — expose the runtime to an editor over the Agent Client
//! Protocol on stdio.
//!
//! The protocol owns stdout, so nothing else may write there: the banner is
//! suppressed for this command and all diagnostics go to stderr. The server runs
//! until the editor closes the stream.

use lightagent_acp::AcpServer;
use lightagent_core::{ConfigStore, LightagentPaths, ProfileId, ProfileStore};
use lightagent_store::SessionStore;

/// Serve ACP over stdin/stdout until end of input.
pub async fn run() -> Result<(), String> {
    let manager = crate::serve::build_run_manager().await?;
    let paths = LightagentPaths::resolve().map_err(|error| error.to_string())?;
    let context_limit = ConfigStore::at(&paths)
        .load()
        .map_err(|error| error.to_string())?
        .runtime
        .n_ctx
        .unwrap_or(4_096) as usize;
    let profiles = ProfileStore::new(paths.root());
    let active = profiles
        .active()
        .map_err(|error| error.to_string())?
        .map(Ok)
        .unwrap_or_else(|| ProfileId::new("default"))
        .map_err(|error| error.to_string())?;
    let sessions = SessionStore::at_profile(&profiles.handle(&active));
    AcpServer::new(manager)
        .with_context_limit(context_limit)
        .with_session_store(sessions, active.as_str())
        .with_profile_store(profiles)
        .serve(tokio::io::stdin(), tokio::io::stdout())
        .await;
    Ok(())
}
