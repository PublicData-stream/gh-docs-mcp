//! Native Git identities and byte coordinates shared with the catalog.

use std::{fmt, ops::Range, str::FromStr};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RepositoryId(Uuid);

impl RepositoryId {
    pub fn new(value: Uuid) -> Result<Self, &'static str> {
        if value.get_version_num() == 4 && value.get_variant() == uuid::Variant::RFC4122 {
            return Ok(Self(value));
        }
        Err("expected an RFC 4122 UUIDv4")
    }

    pub fn uuid(self) -> Uuid {
        self.0
    }
}

impl FromStr for RepositoryId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let uuid = Uuid::parse_str(value).map_err(|_| "invalid repository UUID")?;
        if uuid.to_string() != value {
            return Err("expected a canonical repository UUID");
        }
        Self::new(uuid)
    }
}

impl fmt::Display for RepositoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    pub const fn digest_len(self) -> usize {
        match self {
            Self::Sha1 => 20,
            Self::Sha256 => 32,
        }
    }
}

impl FromStr for ObjectFormat {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "sha1" => Ok(Self::Sha1),
            "sha256" => Ok(Self::Sha256),
            _ => Err("unsupported object format"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Oid {
    Sha1([u8; 20]),
    Sha256([u8; 32]),
}

impl Oid {
    pub fn from_bytes(format: ObjectFormat, bytes: &[u8]) -> Result<Self, &'static str> {
        match format {
            ObjectFormat::Sha1 => bytes.try_into().map(Self::Sha1),
            ObjectFormat::Sha256 => bytes.try_into().map(Self::Sha256),
        }
        .map_err(|_| "invalid native OID length")
    }

    pub fn parse(format: ObjectFormat, hex: &[u8]) -> Result<Self, &'static str> {
        if hex.len() != format.digest_len() * 2 {
            return Err("expected a full native OID");
        }
        fn nibble(byte: u8) -> Result<u8, &'static str> {
            match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err("expected lowercase hexadecimal"),
            }
        }
        let mut bytes = [0; 32];
        for (pair, byte) in hex.as_chunks::<2>().0.iter().zip(&mut bytes) {
            *byte = nibble(pair[0])? * 16 + nibble(pair[1])?;
        }
        Self::from_bytes(format, &bytes[..format.digest_len()])
    }

    pub const fn format(self) -> ObjectFormat {
        match self {
            Self::Sha1(_) => ObjectFormat::Sha1,
            Self::Sha256(_) => ObjectFormat::Sha256,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Sha1(bytes) => bytes,
            Self::Sha256(bytes) => bytes,
        }
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.bytes() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectType {
    Blob,
    Tree,
    Commit,
    Tag,
}

impl ObjectType {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Tree => "tree",
            Self::Commit => "commit",
            Self::Tag => "tag",
        }
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        match bytes {
            b"blob" => Ok(Self::Blob),
            b"tree" => Ok(Self::Tree),
            b"commit" => Ok(Self::Commit),
            b"tag" => Ok(Self::Tag),
            _ => Err("unsupported Git object type"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RefName(Vec<u8>);

impl RefName {
    pub fn new(bytes: Vec<u8>) -> Result<Self, &'static str> {
        if !bytes.starts_with(b"refs/")
            || bytes.len() > 4096
            || bytes.ends_with(b".")
            || bytes.windows(2).any(|pair| matches!(pair, b".." | b"@{"))
            || bytes
                .iter()
                .any(|&byte| byte <= b' ' || b"~^:?*[\\\x7f".contains(&byte))
            || bytes
                .split(|&byte| byte == b'/')
                .any(|part| part.is_empty() || part.starts_with(b".") || part.ends_with(b".lock"))
        {
            return Err("invalid full Git ref name");
        }
        Ok(Self(bytes))
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(
    tag = "encoding",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum BytePath {
    Utf8(String),
    Base64(String),
}

/// Encode a name whose byte grammar is validated by its caller.
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(
    tag = "encoding",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ByteString {
    Utf8(String),
    Base64(String),
}

impl ByteString {
    pub fn encode(bytes: &[u8]) -> Self {
        match std::str::from_utf8(bytes) {
            Ok(text) => Self::Utf8(text.to_owned()),
            Err(_) => Self::Base64(STANDARD.encode(bytes)),
        }
    }

    pub fn decode(&self, max: usize) -> Result<Vec<u8>, &'static str> {
        match self {
            Self::Utf8(text) if text.len() <= max => Ok(text.as_bytes().to_vec()),
            Self::Base64(text) if text.len() <= max.div_ceil(3) * 4 => {
                let bytes = STANDARD.decode(text).map_err(|_| "invalid base64")?;
                if bytes.len() > max
                    || STANDARD.encode(&bytes) != *text
                    || std::str::from_utf8(&bytes).is_ok()
                {
                    return Err("invalid or noncanonical byte string");
                }
                Ok(bytes)
            }
            _ => Err("byte string exceeds limit"),
        }
    }
}

const MAX_PATH_BYTES: usize = 4096;

impl BytePath {
    pub fn encode(bytes: &[u8]) -> Result<Self, &'static str> {
        validate_path(bytes)?;
        Ok(match std::str::from_utf8(bytes) {
            Ok(text) => Self::Utf8(text.to_owned()),
            Err(_) => Self::Base64(STANDARD.encode(bytes)),
        })
    }

    pub fn decode(&self) -> Result<Vec<u8>, &'static str> {
        let bytes = match self {
            Self::Utf8(text) => {
                validate_path(text.as_bytes())?;
                return Ok(text.as_bytes().to_vec());
            }
            Self::Base64(text) => {
                if text.len() > MAX_PATH_BYTES.div_ceil(3) * 4 {
                    return Err("Git path exceeds byte limit");
                }
                let bytes = STANDARD.decode(text).map_err(|_| "invalid base64 path")?;
                if STANDARD.encode(&bytes) != *text || std::str::from_utf8(&bytes).is_ok() {
                    return Err("noncanonical base64 path");
                }
                bytes
            }
        };
        validate_path(&bytes)?;
        Ok(bytes)
    }
}

pub fn validate_component(component: &[u8]) -> Result<(), &'static str> {
    if matches!(component, b"" | b"." | b"..")
        || component.len() > MAX_PATH_BYTES
        || component.contains(&0)
        || component.contains(&b'/')
    {
        return Err("invalid Git tree component");
    }
    Ok(())
}

fn validate_path(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() > MAX_PATH_BYTES {
        return Err("Git path exceeds byte limit");
    }
    for component in bytes.split(|&byte| byte == b'/') {
        validate_component(component)?;
    }
    Ok(())
}

pub fn checked_range(start: u64, end: u64, size: usize) -> Result<Range<usize>, &'static str> {
    let start = usize::try_from(start).map_err(|_| "invalid byte range")?;
    let end = usize::try_from(end).map_err(|_| "invalid byte range")?;
    if start > end || end > size {
        return Err("invalid byte range");
    }
    Ok(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_native_identity_and_raw_coordinates() {
        assert!(
            "12345678-1234-4234-8234-123456789abc"
                .parse::<RepositoryId>()
                .is_ok()
        );
        assert!(
            "12345678-1234-4234-1234-123456789abc"
                .parse::<RepositoryId>()
                .is_err()
        );
        assert!(Oid::parse(ObjectFormat::Sha1, &[b'A'; 40]).is_err());
        assert!(Oid::parse(ObjectFormat::Sha256, &[b'a'; 40]).is_err());
        let path = b"dir/\xff";
        assert_eq!(BytePath::encode(path).unwrap().decode().unwrap(), path);
        assert!(BytePath::Base64("YQ==".into()).decode().is_err());
        assert!(BytePath::encode(b"dir/../name").is_err());
        assert!(RefName::new(b"refs/heads/hidden.lock".to_vec()).is_err());
        assert!(RefName::new(b"refs/heads/raw-\xff".to_vec()).is_ok());
        assert_eq!(checked_range(3, 3, 3), Ok(3..3));
        assert!(checked_range(3, u64::MAX, 3).is_err());
    }
}
