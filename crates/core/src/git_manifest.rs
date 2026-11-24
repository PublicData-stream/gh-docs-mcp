//! Deterministic version 1 Git manifest encoding and domain-separated digests.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub enum Check {
    OfflineFixture,
    HttpsGit,
}

pub struct Manifest {
    pub json: String,
    pub logical: [u8; 32],
    pub observed: [u8; 32],
}

pub fn encode(content: Value, check: Check, timestamp: &str) -> Manifest {
    let verification = match check {
        Check::OfflineFixture => "fixture_checks",
        Check::HttpsGit => "https_git_checks",
    };
    let logical = digest(
        b"gh-docs-manifest-content-v1\0",
        content.to_string().as_bytes(),
    );
    let json = json!([
        "gh-docs-manifest",
        1,
        content,
        ["verified", verification, timestamp]
    ])
    .to_string();
    let observed = observation_digest(json.as_bytes());
    Manifest {
        json,
        logical,
        observed,
    }
}

pub fn observation_digest(bytes: &[u8]) -> [u8; 32] {
    digest(b"gh-docs-manifest-observation-v1\0", bytes)
}

fn digest(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(bytes);
    hash.finalize().into()
}
