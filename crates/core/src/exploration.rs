//! Reviewed permissions for exploration output.

use crate::git::BytePath;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplorationApproval {
    pub version: u8,
    pub metadata_notice: String,
    pub scope: Scope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outputs: Option<Outputs>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope {
    Whole { notice: String },
    Paths { grants: Vec<PathGrant> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathGrant {
    pub path: BytePath,
    pub subtree: bool,
    pub notice: String,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Outputs {
    pub search: bool,
    pub history: bool,
}

#[derive(Debug)]
pub struct Policy {
    pub metadata_notice: String,
    pub scope: ApprovedScope,
    pub outputs: Outputs,
}

#[derive(Debug)]
pub enum ApprovedScope {
    Whole(String),
    Paths(Vec<Grant>),
}

#[derive(Debug)]
pub struct Grant {
    pub path: Vec<u8>,
    pub subtree: bool,
    pub notice: String,
}

pub fn beneath(path: &[u8], directory: &[u8]) -> bool {
    directory.is_empty()
        || path
            .strip_prefix(directory)
            .is_some_and(|suffix| suffix.starts_with(b"/"))
}

fn validate_notice(text: &str) -> Result<(), &'static str> {
    if text.trim().is_empty() || text.len() > 4096 || text.contains('\0') {
        return Err("missing or invalid exploration notice");
    }
    Ok(())
}

impl ExplorationApproval {
    pub fn validate(&self) -> Result<Policy, &'static str> {
        if !matches!((self.version, self.outputs), (1, None) | (2, _)) {
            return Err("unsupported exploration approval version");
        }
        validate_notice(&self.metadata_notice)?;
        let scope = match &self.scope {
            Scope::Whole { notice } => {
                validate_notice(notice)?;
                ApprovedScope::Whole(notice.clone())
            }
            Scope::Paths { grants } if (1..=64).contains(&grants.len()) => {
                let mut paths: Vec<Grant> = Vec::with_capacity(grants.len());
                for grant in grants {
                    validate_notice(&grant.notice)?;
                    let path = grant.path.decode()?;
                    if paths.iter().any(|prior| {
                        path == prior.path
                            || beneath(&path, &prior.path)
                            || beneath(&prior.path, &path)
                    }) {
                        return Err("overlapping exploration grants");
                    }
                    paths.push(Grant {
                        path,
                        subtree: grant.subtree,
                        notice: grant.notice.clone(),
                    });
                }
                ApprovedScope::Paths(paths)
            }
            Scope::Paths { .. } => return Err("invalid exploration grant count"),
        };
        Ok(Policy {
            metadata_notice: self.metadata_notice.clone(),
            scope,
            outputs: self.outputs.unwrap_or_default(),
        })
    }
}

impl ApprovedScope {
    pub fn notice(&self, path: &[u8], tree: bool) -> Option<&str> {
        match self {
            Self::Whole(notice) => Some(notice),
            Self::Paths(grants) => grants
                .iter()
                .find(|grant| {
                    (path == grant.path && tree == grant.subtree)
                        || (grant.subtree && beneath(path, &grant.path))
                })
                .map(|grant| grant.notice.as_str()),
        }
    }

    pub fn descendants<'a>(&'a self, directory: &'a [u8]) -> impl Iterator<Item = &'a Grant> {
        let grants = match self {
            Self::Whole(_) => &[][..],
            Self::Paths(grants) => grants.as_slice(),
        };
        grants
            .iter()
            .filter(move |grant| beneath(&grant.path, directory))
    }
}
