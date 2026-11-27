use std::{collections::BTreeSet, net::IpAddr, path::Path};

use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use docs_mcp_core::{
    Result,
    exploration::{ExplorationApproval, GLOB_APPROVAL_VERSION},
    git::{ObjectFormat, RefName, RepositoryId},
};
use serde::{Deserialize, Serialize};
use tokio_postgres::Client;
use uuid::Uuid;

pub const MAX_REFS: usize = 1024;
pub const MIB: u64 = 1024 * 1024;
pub const MAX_PACK: u64 = 256 * MIB;
pub const MAX_DOWNLOAD: u64 = 384 * MIB;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub pack_bytes: u64,
    pub response_bytes: u64,
    pub objects: usize,
    pub deadline_seconds: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub repository: String,
    pub source: String,
    pub format: String,
    pub version: i64,
    pub url: String,
    pub hosts: Vec<String>,
    pub required_refs: Vec<ScopeName>,
    pub optional_refs: Vec<ScopeName>,
    pub prefixes: Vec<ScopeName>,
    pub reviewer: String,
    pub reviewed_at: String,
    pub full_closure_approved: bool,
    pub output_scope: String,
    pub license_evidence: String,
    pub notice_evidence: String,
    pub limits: Limits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exploration: Option<ExplorationApproval>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(try_from = "ScopeInput", into = "ScopeInput")]
pub struct ScopeName(Vec<u8>);

impl ScopeName {
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged, deny_unknown_fields)]
enum ScopeInput {
    Text(String),
    Bytes { base64: String },
}

impl TryFrom<ScopeInput> for ScopeName {
    type Error = &'static str;

    fn try_from(value: ScopeInput) -> std::result::Result<Self, Self::Error> {
        Ok(Self(match value {
            ScopeInput::Text(value) => value.into_bytes(),
            ScopeInput::Bytes { base64 } => {
                let bytes = STANDARD
                    .decode(&base64)
                    .map_err(|_| "invalid base64 ref scope")?;
                if STANDARD.encode(&bytes) != base64 {
                    return Err("noncanonical base64 ref scope");
                }
                bytes
            }
        }))
    }
}

impl From<ScopeName> for ScopeInput {
    fn from(value: ScopeName) -> Self {
        match String::from_utf8(value.0) {
            Ok(text) => Self::Text(text),
            Err(error) => Self::Bytes {
                base64: STANDARD.encode(error.as_bytes()),
            },
        }
    }
}

fn namespace(reference: &[u8]) -> bool {
    reference.starts_with(b"refs/heads/") || reference.starts_with(b"refs/tags/")
}

impl Approval {
    pub fn host(&self) -> &str {
        self.url[8..].split_once('/').expect("validated URL").0
    }

    pub fn allows(&self, reference: &RefName) -> bool {
        self.required_refs
            .iter()
            .chain(&self.optional_refs)
            .any(|exact| exact.bytes() == reference.bytes())
            || self
                .prefixes
                .iter()
                .any(|prefix| reference.bytes().starts_with(prefix.bytes()))
    }

    pub fn validate(&self) -> Result<(RepositoryId, Uuid, ObjectFormat)> {
        if let Some(exploration) = &self.exploration {
            exploration.validate()?;
        }
        let repo = self.repository.parse()?;
        let source: RepositoryId = self.source.parse()?;
        let format = self.format.parse()?;
        let host = canonical_host(&self.url)?;
        if self.hosts.is_empty()
            || self.hosts.len() > 16
            || !self.hosts.iter().any(|x| x == host)
            || self.hosts.iter().any(|x| !valid_host(x))
            || self.hosts.iter().collect::<BTreeSet<_>>().len() != self.hosts.len()
            || self.version < 1
            || !self.full_closure_approved
        {
            return Err("invalid source authority or approval".into());
        }
        let reviewed = DateTime::parse_from_rfc3339(&self.reviewed_at)?;
        if reviewed > Utc::now() {
            return Err("source review is in the future".into());
        }
        if [
            &self.reviewer,
            &self.output_scope,
            &self.license_evidence,
            &self.notice_evidence,
        ]
        .iter()
        .any(|value| value.trim().is_empty() || value.len() > 8192 || value.contains('\0'))
        {
            return Err("source review evidence is missing or invalid".into());
        }
        let exact = self.required_refs.iter().chain(&self.optional_refs);
        let count = exact.clone().count() + self.prefixes.len();
        if count == 0
            || count > MAX_REFS
            || exact.clone().collect::<BTreeSet<_>>().len() != exact.clone().count()
            || self.prefixes.iter().collect::<BTreeSet<_>>().len() != self.prefixes.len()
        {
            return Err("invalid ref policy size or duplicate entries".into());
        }
        for name in exact {
            if !namespace(name.bytes()) {
                return Err("unsupported approved ref namespace".into());
            }
            RefName::new(name.bytes().to_vec())?;
        }
        for prefix in &self.prefixes {
            if !namespace(prefix.bytes()) || !prefix.bytes().ends_with(b"/") {
                return Err("invalid approved ref prefix".into());
            }
            let mut name = prefix.bytes().to_vec();
            name.extend_from_slice(b"x");
            RefName::new(name)?;
        }
        let limits = &self.limits;
        if !(1..=MAX_PACK).contains(&limits.pack_bytes)
            || !(1..=MAX_DOWNLOAD).contains(&limits.response_bytes)
            || !(1..=500_000).contains(&limits.objects)
            || !(1..=900).contains(&limits.deadline_seconds)
        {
            return Err("source limits exceed the Phase 2 envelope".into());
        }
        Ok((repo, source.uuid(), format))
    }
}

fn valid_host(host: &str) -> bool {
    host.len() <= 253
        && host.contains('.')
        && host.parse::<IpAddr>().is_err()
        && host
            .rsplit('.')
            .next()
            .is_some_and(|part| part.bytes().any(|b| b.is_ascii_lowercase()))
        && host.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

fn canonical_host(url: &str) -> Result<&str> {
    let (host, path) = url
        .strip_prefix("https://")
        .and_then(|s| s.split_once('/'))
        .ok_or("source URL must use canonical HTTPS")?;
    if url.len() > 2048
        || !valid_host(host)
        || path.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
        })
    {
        return Err("source URL must use a canonical DNS name and path".into());
    }
    Ok(host)
}

pub async fn register(client: &Client, file: &Path) -> Result<()> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(file)?
        .take(131_073)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 131_072 {
        return Err("approval document exceeds 128 KiB".into());
    }
    let approval: Approval = serde_json::from_slice(&bytes)?;
    let (repo, source, format) = approval.validate()?;
    if approval
        .exploration
        .as_ref()
        .is_some_and(|exploration| exploration.version == GLOB_APPROVAL_VERSION)
    {
        let migrated: bool = client
            .query_one(
                "SELECT EXISTS (SELECT FROM git_catalog.migrations WHERE version = 8)",
                &[],
            )
            .await?
            .get(0);
        if !migrated {
            return Err(
                "apply Git catalog migration 0008 before registering glob approvals".into(),
            );
        }
    }
    let exact: Vec<_> = approval
        .required_refs
        .iter()
        .chain(&approval.optional_refs)
        .map(ScopeName::bytes)
        .collect();
    let value = serde_json::to_value(&approval)?;
    client
        .query_one(
            "SELECT git_catalog.approve_source($1,$2,$3,$4,$5,$6,$7)",
            &[
                &repo.uuid(),
                &source,
                &format.name(),
                &approval.url,
                &approval.version,
                &exact,
                &value,
            ],
        )
        .await?;
    println!(
        "{}",
        serde_json::json!({"source": source, "policy_version": approval.version})
    );
    Ok(())
}

pub async fn load(client: &Client, repo: RepositoryId) -> Result<Approval> {
    let row = client
        .query_opt(
            "SELECT s.review_evidence FROM git_catalog.repository_sources s
        JOIN git_catalog.repositories r USING (repository_id)
        WHERE s.repository_id=$1 AND r.approval_status='approved' AND s.acquisition_method='https'",
            &[&repo.uuid()],
        )
        .await?
        .ok_or("repository has no active HTTPS approval")?;
    let approval: Approval = serde_json::from_value(row.get(0))?;
    if approval.validate()?.0 != repo {
        return Err("source approval repository mismatch".into());
    }
    Ok(approval)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_url_rejects_ambiguous_and_local_forms() {
        assert_eq!(
            canonical_host("https://git.example.org/owner/repo.git").unwrap(),
            "git.example.org"
        );
        for value in [
            "http://git.example.org/repo",
            "https://127.0.0.1/repo",
            "https://[::1]/repo",
            "https://user@git.example.org/repo",
            "https://git.example.org:443/repo",
            "https://git.example.org/repo?query",
            "https://git.example.org/../repo",
            "https://git.example.org/%2e%2e/repo",
            "https://Git.example.org/repo",
            "ssh://git.example.org/repo",
        ] {
            assert!(canonical_host(value).is_err(), "{value}");
        }
    }
}
