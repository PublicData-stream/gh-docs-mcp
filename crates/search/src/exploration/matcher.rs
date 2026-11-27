use docs_mcp_core::path_glob::{MAX_GLOB_BYTES, PathGlob};

use super::{Error, request::Pattern};

pub(super) enum Matcher {
    Literal(Vec<u8>),
    Glob(PathGlob),
}

impl Matcher {
    pub fn compile(pattern: Pattern) -> Result<Self, Error> {
        match pattern {
            Pattern::Literal(value) => {
                let bytes = value
                    .decode(MAX_GLOB_BYTES)
                    .map_err(|_| Error::InvalidArgument)?;
                if bytes.is_empty() || bytes.contains(&0) {
                    return Err(Error::InvalidArgument);
                }
                Ok(Self::Literal(bytes))
            }
            Pattern::Glob(value) => {
                let bytes = value
                    .decode(MAX_GLOB_BYTES)
                    .map_err(|_| Error::InvalidArgument)?;
                Ok(Self::Glob(
                    PathGlob::compile(&bytes).map_err(|_| Error::InvalidArgument)?,
                ))
            }
        }
    }

    pub fn matches(&self, path: &[u8]) -> bool {
        match self {
            Self::Literal(literal) => path.windows(literal.len()).any(|bytes| bytes == literal),
            Self::Glob(pattern) => pattern.matches(path),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use docs_mcp_core::git::ByteString;

    #[test]
    fn glob_is_anchored_and_operates_on_bytes() {
        let glob =
            |text: &str| Matcher::compile(Pattern::Glob(ByteString::Utf8(text.into()))).unwrap();
        assert!(glob("a/**/?.txt").matches(b"a/\xff.txt"));
        assert!(glob("a/**/?.txt").matches(b"a/cache/\xff.txt"));
        assert!(!glob("a/**/?.txt").matches("a/é.txt".as_bytes()));
        assert!(!glob("a/*.txt").matches(b"a/cache/one.txt"));
        assert!(!glob("a/**").matches(b"b/a/test"));
    }
}
