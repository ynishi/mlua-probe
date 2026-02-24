mod handler;

use rmcp::{transport::stdio, ServiceExt};
use tracing_subscriber::{self, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Log to stderr — stdout is reserved for MCP JSON-RPC.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!("mlua-probe-mcp server starting");

    let server = handler::DebugMcpHandler::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
