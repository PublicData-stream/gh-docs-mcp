use docs_mcp_core::git::{BytePath, ByteString, ObjectFormat, Oid, RepositoryId};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::Error;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "tool", content = "arguments", deny_unknown_fields)]
pub enum Request {
    #[serde(rename = "repository.search")]
    RepositorySearch(RepositorySearch),
    #[serde(rename = "repository.info")]
    RepositoryInfo(RepositoryInfo),
    #[serde(rename = "git.refs")]
    Refs(Refs),
    #[serde(rename = "git.resolve")]
    Resolve(Resolve),
    #[serde(rename = "git.ls_tree")]
    ListTree(ListTree),
    #[serde(rename = "git.find")]
    Find(Find),
    #[serde(rename = "git.read")]
    Read(Read),
    #[serde(rename = "git.rg")]
    Rg(Rg),
    #[serde(rename = "git.log")]
    Log(Log),
    #[serde(rename = "git.show")]
    Show(Show),
    #[serde(rename = "git.diff")]
    Diff(Diff),
    #[serde(rename = "git.blame")]
    Blame(Blame),
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Page {
    pub limit: Option<u16>,
    pub cursor: Option<String>,
}

impl Page {
    pub(super) fn limit(&self) -> Result<usize, Error> {
        let limit = self.limit.unwrap_or(20);
        if !(1..=100).contains(&limit) {
            return Err(Error::InvalidArgument);
        }
        Ok(limit.into())
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositorySearch {
    #[serde(default)]
    pub query: String,
    #[serde(default)]
    pub page: Page,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryInfo {
    pub repository_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Refs {
    pub repository_id: String,
    pub generation_id: Option<String>,
    #[serde(default)]
    pub page: Page,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Resolve {
    pub repository_id: String,
    pub generation_id: Option<String>,
    pub selector: Selector,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Selector {
    Ref(ByteString),
    Oid(String),
    Default,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub repository_id: String,
    pub generation_id: String,
    pub object_format: String,
    pub commit_oid: String,
}

impl Context {
    pub(super) fn coordinates(&self) -> Result<(RepositoryId, i64, ObjectFormat, Oid), Error> {
        let format: ObjectFormat = self
            .object_format
            .parse()
            .map_err(|_| Error::UnsupportedFormat)?;
        Ok((
            repository(&self.repository_id)?,
            generation(&self.generation_id)?,
            format,
            Oid::parse(format, self.commit_oid.as_bytes()).map_err(|_| Error::InvalidArgument)?,
        ))
    }
}

pub(super) fn repository(value: &str) -> Result<RepositoryId, Error> {
    value.parse().map_err(|_| Error::InvalidArgument)
}

pub(super) fn generation(value: &str) -> Result<i64, Error> {
    let number: i64 = value.parse().map_err(|_| Error::InvalidArgument)?;
    if number < 0 || number.to_string() != value {
        return Err(Error::InvalidArgument);
    }
    Ok(number)
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTree {
    pub context: Context,
    pub root: Option<BytePath>,
    pub depth: Option<u8>,
    #[serde(default)]
    pub page: Page,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Find {
    pub context: Context,
    pub root: Option<BytePath>,
    pub depth: Option<u8>,
    pub pattern: Pattern,
    #[serde(default)]
    pub page: Page,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Pattern {
    Literal(ByteString),
    Glob(ByteString),
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Read {
    pub context: Context,
    pub path: BytePath,
    pub range: Option<ByteRange>,
    #[serde(default)]
    pub encoding: Encoding,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ByteRange {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Utf8,
    Base64,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rg {
    pub context: Context,
    pub pattern: String,
    #[serde(default)]
    pub literal: bool,
    #[serde(default)]
    pub ignore_case: bool,
    pub root: Option<BytePath>,
    pub path_glob: Option<ByteString>,
    #[serde(default)]
    pub context_lines: u8,
    #[serde(default)]
    pub page: Page,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Log {
    pub context: Context,
    pub path: Option<BytePath>,
    #[serde(default)]
    pub page: Page,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Show {
    pub context: Context,
    pub parent: Option<u32>,
    pub path: Option<BytePath>,
    pub context_lines: Option<u8>,
    #[serde(default)]
    pub page: Page,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Diff {
    pub base: Context,
    pub head: Context,
    pub path: Option<BytePath>,
    pub context_lines: Option<u8>,
    #[serde(default)]
    pub page: Page,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Blame {
    pub context: Context,
    pub path: BytePath,
    pub line_range: Option<ByteRange>,
    #[serde(default)]
    pub page: Page,
}
