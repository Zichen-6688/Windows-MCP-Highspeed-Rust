//! windows-mcp-highspeed: MCP server for Windows desktop automation over the
//! UI Automation accessibility tree. Stdio speaks MCP JSON-RPC (newline
//! delimited); all logs go to stderr.

mod error;
mod highlight;
mod locator;
mod model;
mod server;
mod uia;

use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // stdout is reserved for MCP JSON-RPC; logs must never touch it.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let server = server::UiaServer::new()?;
    tracing::info!("windows-mcp-highspeed {} starting", env!("CARGO_PKG_VERSION"));

    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
