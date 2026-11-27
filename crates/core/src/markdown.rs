//! Reviewed Rustdoc input and output coordinates shared by Sync and Serve.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    Result,
    git::{BytePath, RefName, RepositoryId},
    path_glob::PathGlob,
};

pub const FORMAT: u32 = 61;
pub const DOCUMENT_BYTES: usize = 1024 * 1024;
pub const SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
pub const DOCUMENTS: usize = 50_000;
pub const JSON_BYTES: usize = 128 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    pub source_id: String,
    pub policy_version: i64,
    pub input: Input,
    pub reviewer: String,
    pub license_evidence: String,
    pub notice: String,
    #[serde(default)]
    pub search: bool,
    pub grants: Vec<Grant>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Input {
    GitRustdoc {
        repository_id: String,
        reference: String,
        workspace: Option<String>,
        package: String,
        target: String,
    },
    DocsrsRustdoc {
        package: String,
        version: String,
        target: String,
        format: u32,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    pub source_path_glob: String,
    pub path_glob: String,
}

pub struct Grants(Vec<(PathGlob, PathGlob)>);

impl Grants {
    pub fn allows(&self, source_path: &str, path: &str) -> bool {
        self.0.iter().any(|(source, output)| {
            source.matches(source_path.as_bytes()) && output.matches(path.as_bytes())
        })
    }
}

impl Input {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::GitRustdoc { .. } => "git_rustdoc",
            Self::DocsrsRustdoc { .. } => "docsrs_rustdoc",
        }
    }

    pub fn package_target(&self) -> (&str, &str) {
        match self {
            Self::GitRustdoc {
                package, target, ..
            }
            | Self::DocsrsRustdoc {
                package, target, ..
            } => (package, target),
        }
    }
}

impl Approval {
    pub fn validate(&self) -> Result<Grants> {
        let (package, target) = self.input.package_target();
        self.source_id.parse::<RepositoryId>()?;
        if self.policy_version < 1
            || !slug(package)
            || !slug(target)
            || [&self.reviewer, &self.license_evidence, &self.notice]
                .iter()
                .any(|value| value.trim().is_empty() || value.len() > 4096 || value.contains('\0'))
            || !(1..=64).contains(&self.grants.len())
        {
            return Err("invalid Markdown source approval".into());
        }
        match &self.input {
            Input::GitRustdoc {
                repository_id,
                reference,
                workspace,
                target,
                ..
            } => {
                repository_id.parse::<RepositoryId>()?;
                RefName::new(reference.as_bytes().to_vec())?;
                if !(reference.starts_with("refs/heads/") || reference.starts_with("refs/tags/"))
                    || workspace.as_ref().is_some_and(|path| !clean_path(path))
                    || target != "x86_64-unknown-linux-gnu"
                {
                    return Err("invalid Git Rustdoc build coordinates".into());
                }
            }
            Input::DocsrsRustdoc {
                version, format, ..
            } => {
                if !exact_version(version) || *format != FORMAT {
                    return Err("unsupported docs.rs release coordinates".into());
                }
            }
        }
        self.grants
            .iter()
            .map(|grant| {
                Ok((
                    PathGlob::compile(grant.source_path_glob.as_bytes())?,
                    PathGlob::compile(grant.path_glob.as_bytes())?,
                ))
            })
            .collect::<Result<Vec<_>>>()
            .map(Grants)
    }
}

pub fn slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub fn exact_version(value: &str) -> bool {
    value.len() <= 96
        && semver::Version::parse(value).is_ok_and(|parsed| parsed.to_string() == value)
}

pub fn clean_path(path: &str) -> bool {
    BytePath::encode(path.as_bytes()).is_ok()
        && path.len() <= 1024
        && !path.contains('\\')
        && !path.contains(':')
        && !path.chars().any(char::is_control)
        && !path
            .split('/')
            .any(|part| part.eq_ignore_ascii_case(".git"))
}

pub fn digest(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        write!(&mut text, "{byte:02x}").expect("write to string");
    }
    text
}

pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub path: String,
    pub source_path: String,
    pub item: String,
    pub kind: String,
    pub line: u64,
    pub markdown: String,
}

impl Document {
    pub fn valid(&self) -> bool {
        clean_path(&self.path)
            && self.path.starts_with("rustdoc/")
            && self.path.ends_with(".md")
            && clean_path(&self.source_path)
            && self.source_path.ends_with(".rs")
            && !self.item.is_empty()
            && self.item.len() <= 1024
            && !self.item.chars().any(char::is_control)
            && slug(&self.kind)
            && self.line > 0
            && self.line <= i64::MAX as u64
            && !self.markdown.is_empty()
            && self.markdown.len() <= DOCUMENT_BYTES
            && !self.markdown.contains('\0')
    }
}
