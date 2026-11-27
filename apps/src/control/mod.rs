// Adapted from rfc-mcp-docker. See docs/RFC-MCP-DOCKER-LICENSE.
//! Private, single-host control traffic. The public MCP router does not use this module.

mod auth;
mod wire;

use docs_mcp_core::git::RepositoryId;
use serde::{Deserialize, Serialize};
use std::{
    io,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub use auth::Keys;
pub use wire::{Client, address, serve};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum Link {
    FeedSync,
    SyncServe,
}

impl Link {
    // These role identifiers are part of the signed v1 control envelope.
    fn roles(self) -> (&'static str, &'static str) {
        match self {
            Self::FeedSync => ("gh-docs-feed", "gh-docs-sync"),
            Self::SyncServe => ("gh-docs-sync", "gh-docs-serve"),
        }
    }

    fn key_prefix(self) -> &'static str {
        match self {
            Self::FeedSync => "DOCS_MCP_FEED_SYNC",
            Self::SyncServe => "DOCS_MCP_SYNC_SERVE",
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Route {
    Identify,
    Sync,
    Generation,
}

impl Route {
    fn path(self) -> &'static str {
        match self {
            Self::Identify => "/internal/v1/identify",
            Self::Sync => "/internal/v1/sync-requested",
            Self::Generation => "/internal/v1/generation-committed",
        }
    }

    fn for_link(path: &str, link: Link) -> Option<Self> {
        let business = match link {
            Link::FeedSync => Self::Sync,
            Link::SyncServe => Self::Generation,
        };
        [Self::Identify, business]
            .into_iter()
            .find(|route| route.path() == path)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    FeedRefs,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRequested {
    pub event_id: Uuid,
    pub repository_id: Uuid,
    pub observation_digest: String,
    pub observed_at: u64,
    pub reason: Reason,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationCommitted {
    pub event_id: Uuid,
    pub repository_id: Uuid,
    pub generation_id: String,
    pub publication_sequence: String,
}

pub enum Command {
    Sync(SyncRequested),
    Generation(GenerationCommitted),
}

impl Command {
    fn decode(route: Route, body: &[u8]) -> Result<Self> {
        match route {
            Route::Identify => Err(invalid("unexpected control operation").into()),
            Route::Sync => {
                let event: SyncRequested = serde_json::from_slice(body)?;
                validate_digest(&event.observation_digest)?;
                RepositoryId::new(event.repository_id)?;
                if (event.event_id.get_version_num() != 7
                    || event.event_id.get_variant() != uuid::Variant::RFC4122)
                    || event.observed_at > 253_402_300_799
                {
                    return Err(invalid("invalid feed observation identity or time").into());
                }
                Ok(Self::Sync(event))
            }
            Route::Generation => {
                let event: GenerationCommitted = serde_json::from_slice(body)?;
                RepositoryId::new(event.repository_id)?;
                let _: i64 = auth::decimal(&event.generation_id)?.try_into()?;
                let sequence: i64 = auth::decimal(&event.publication_sequence)?.try_into()?;
                if sequence == 0
                    || (event.event_id.get_version_num() != 7
                        || event.event_id.get_variant() != uuid::Variant::RFC4122)
                {
                    return Err(invalid("invalid generation event").into());
                }
                Ok(Self::Generation(event))
            }
        }
    }

    fn event_id(&self) -> Uuid {
        match self {
            Self::Sync(event) => event.event_id,
            Self::Generation(event) => event.event_id,
        }
    }
}

fn validate_digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid("invalid observation or manifest digest").into());
    }
    Ok(())
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Accepted,
    Observed,
    Superseded,
}

#[derive(Clone, Copy)]
pub enum Rejection {
    Conflict,
    Unavailable,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Ack {
    event_id: Uuid,
    outcome: Outcome,
}

pub fn unix_seconds() -> io::Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_secs())
}

pub fn jitter(duration: Duration) -> Duration {
    let mut byte = [0_u8; 1];
    if getrandom::fill(&mut byte).is_err() {
        return duration;
    }
    duration.mul_f64(1.0 + f64::from(byte[0]) / 2550.0)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn enabled() -> Result<bool> {
    match std::env::var("DOCS_MCP_CONTROL_ENABLED") {
        Ok(value) if value == "1" => Ok(true),
        Ok(value) if value == "0" => Ok(false),
        Err(std::env::VarError::NotPresent) => Ok(false),
        _ => Err(invalid("invalid control enable setting").into()),
    }
}

pub fn config<T: serde::de::DeserializeOwned>(path: &std::path::Path, max: u64) -> Result<T> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("control configuration must be a regular file").into());
    }
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(invalid("control configuration exceeds its size limit").into());
    }
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn shutdown() -> io::Result<()> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = term.recv() => Ok(()),
    }
}

// A separate fixture image can route synthetic HTTPS to its test server.
pub fn fixture() -> Result<Option<docs_mcp_sync::Fixture>> {
    #[cfg(feature = "phase6-fixtures")]
    if let Ok(endpoint) = std::env::var("DOCS_MCP_PHASE6_UPSTREAM") {
        return Ok(Some(docs_mcp_sync::Fixture {
            address: endpoint.parse()?,
            ca: Some(docs_mcp_core::required("DOCS_MCP_PHASE6_UPSTREAM_CA")?.into()),
            fault: std::env::var("DOCS_MCP_PHASE6_FETCH_FAULT").ok(),
        }));
    }
    Ok(None)
}

/// Select a process interruption in a disposable fixture image.
pub fn fixture_fault() -> Option<String> {
    #[cfg(feature = "phase6-fixtures")]
    if let Some(path) = std::env::var_os("DOCS_MCP_PHASE6_FAULT_FILE")
        && let Ok(point) = std::fs::read_to_string(path)
    {
        return Some(point.trim().to_owned());
    }
    None
}

pub fn fault(label: &str) {
    if fixture_fault().as_deref() == Some(label) {
        std::process::exit(86);
    }
}
