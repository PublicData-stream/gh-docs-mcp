//! Reviewed permissions for exploration output.

use crate::{git::BytePath, path_glob::PathGlob};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

pub const GLOB_APPROVAL_VERSION: u8 = 3;

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
    Paths { grants: Vec<GrantInput> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GrantInput {
    Path(PathGrant),
    Glob(GlobGrant),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobGrant {
    pub glob: String,
    pub notice: String,
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
    Paths {
        paths: Vec<Grant>,
        globs: Vec<(PathGlob, String)>,
    },
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
        if !matches!(
            (self.version, self.outputs),
            (1, None) | (2 | GLOB_APPROVAL_VERSION, _)
        ) {
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
                let mut globs = Vec::new();
                let mut seen_globs = HashSet::new();
                for grant in grants {
                    let grant = match grant {
                        GrantInput::Path(grant) => grant,
                        GrantInput::Glob(grant) => {
                            if self.version != GLOB_APPROVAL_VERSION {
                                return Err("glob grants require exploration approval version 3");
                            }
                            validate_notice(&grant.notice)?;
                            if !seen_globs.insert(grant.glob.as_str()) {
                                return Err("duplicate exploration glob");
                            }
                            globs.push((
                                PathGlob::compile(grant.glob.as_bytes())?,
                                grant.notice.clone(),
                            ));
                            continue;
                        }
                    };
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
                ApprovedScope::Paths { paths, globs }
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
    pub fn allows_leaf_path(&self, path: &[u8]) -> bool {
        self.direct_notice(path, false).is_some() || self.glob_notice(path).is_some()
    }

    pub fn direct_notice(&self, path: &[u8], tree: bool) -> Option<&str> {
        match self {
            Self::Whole(notice) => Some(notice),
            Self::Paths { paths, .. } => paths
                .iter()
                .find(|grant| {
                    (path == grant.path && tree == grant.subtree)
                        || (grant.subtree && beneath(path, &grant.path))
                })
                .map(|grant| grant.notice.as_str()),
        }
    }

    pub fn glob_notice(&self, path: &[u8]) -> Option<&str> {
        match self {
            Self::Whole(_) => None,
            Self::Paths { globs, .. } => globs
                .iter()
                .find(|(glob, _)| glob.matches(path))
                .map(|(_, notice)| notice.as_str()),
        }
    }

    pub fn can_match_glob_below(&self, directory: &[u8]) -> bool {
        match self {
            Self::Whole(_) => false,
            Self::Paths { globs, .. } => globs
                .iter()
                .any(|(glob, _)| glob.can_match_descendant(directory)),
        }
    }

    pub fn descendants<'a>(&'a self, directory: &'a [u8]) -> impl Iterator<Item = &'a Grant> {
        let grants = match self {
            Self::Whole(_) => &[][..],
            Self::Paths { paths, .. } => paths.as_slice(),
        };
        grants
            .iter()
            .filter(move |grant| beneath(&grant.path, directory))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn approval(version: u8, grants: &[Value]) -> Result<Policy, Box<dyn std::error::Error>> {
        let input: ExplorationApproval = serde_json::from_value(json!({
            "version": version,
            "metadata_notice": "metadata",
            "scope": {"kind": "paths", "grants": grants},
        }))?;
        Ok(input.validate()?)
    }

    #[test]
    fn mixed_grants_preserve_literal_and_glob_order() {
        let policy = approval(
            3,
            &[
                json!({"glob":"docs/**/*.md", "notice":"first"}),
                json!({"path":{"encoding":"utf8","value":"docs/index.md"}, "subtree":false, "notice":"file"}),
                json!({"glob":"docs/setup/*.md", "notice":"second"}),
            ],
        )
        .unwrap();
        assert!(!policy.outputs.search && !policy.outputs.history);
        assert_eq!(
            policy.scope.glob_notice(b"docs/setup/\xff.md"),
            Some("first")
        );
        assert_eq!(
            policy.scope.direct_notice(b"docs/index.md", false),
            Some("file")
        );
        assert_eq!(policy.scope.glob_notice(b"docs/index.MD"), None);
        assert!(!policy.scope.allows_leaf_path(b"other/index.md"));
        assert!(!policy.scope.can_match_glob_below(b"docs-old"));
    }

    #[test]
    fn rejects_malformed_and_legacy_glob_grants() {
        let valid = json!({"glob":"docs/**/*.md", "notice":"reviewed"});
        for version in [1, 2, 4] {
            assert!(approval(version, std::slice::from_ref(&valid)).is_err());
        }
        assert!(approval(3, &[valid.clone(), valid]).is_err());
        for glob in [
            "",
            "/docs/*.md",
            "docs//x",
            "a/./x",
            "a/../x",
            "docs/ab**.md",
            "a\0b",
            &"a".repeat(257),
        ] {
            assert!(approval(3, &[json!({"glob":glob, "notice":"reviewed"})]).is_err());
        }
        assert!(
            approval(
                3,
                &[json!({"glob":"*.md", "subtree":false, "notice":"reviewed"})]
            )
            .is_err()
        );
        let oversized: Vec<_> = (0..65)
            .map(|i| json!({"glob":format!("{i}.md"), "notice":"reviewed"}))
            .collect();
        assert!(approval(3, &oversized).is_err());
    }
}
