//! Shared configuration for the independently deployed roles.

use std::{env, io, net::SocketAddr};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn required(name: &str) -> io::Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {name}")))
}

pub fn health_bind() -> Result<SocketAddr> {
    let address: SocketAddr = required("HEALTH_BIND")?.parse()?;
    if !address.ip().is_loopback() {
        return Err(
            io::Error::new(io::ErrorKind::InvalidInput, "health must bind to loopback").into(),
        );
    }
    Ok(address)
}
pub mod git;
