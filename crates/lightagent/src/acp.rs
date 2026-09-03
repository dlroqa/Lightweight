//! `lightagent acp` — expose the runtime to an editor over the Agent Client
//! Protocol on stdio.
//!
//! The protocol owns stdout, so nothing else may write there: the banner is
//! suppressed for this command and all diagnostics go to stderr. The server runs
//! until the editor closes the stream.

use lightagent_acp::AcpServer;

/// Serve ACP over stdin/stdout until end of input.
pub async fn run() -> Result<(), String> {
    let manager = crate::serve::build_run_manager().await?;
    AcpServer::new(manager)
        .serve(tokio::io::stdin(), tokio::io::stdout())
        .await;
    Ok(())
}
