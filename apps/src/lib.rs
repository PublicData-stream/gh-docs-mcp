//! The Phase 0 entry points share loopback health and role probes.

pub mod control;
pub mod mcp;
pub mod scheduler;

use axum::{Router, http::StatusCode, routing::get};
use docs_mcp_core::{Result, health_bind};
use tokio::net::TcpListener;

pub async fn health_listener() -> Result<TcpListener> {
    Ok(TcpListener::bind(health_bind()?).await?)
}

pub async fn health(listener: TcpListener) -> std::io::Result<()> {
    let health = Router::new().route("/healthz", get(|| async { StatusCode::NO_CONTENT }));
    axum::serve(listener, health).await
}
