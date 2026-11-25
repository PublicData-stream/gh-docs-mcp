use super::{Error, request::Pattern};

pub(super) enum Matcher {
    Literal(Vec<u8>),
    Glob(Vec<Vec<u8>>),
}

impl Matcher {
    pub fn compile(pattern: Pattern) -> Result<Self, Error> {
        let decode = |value: gh_docs_core::git::ByteString| {
            let bytes = value.decode(256).map_err(|_| Error::InvalidArgument)?;
            if bytes.is_empty() || bytes.contains(&0) {
                return Err(Error::InvalidArgument);
            }
            Ok(bytes)
        };
        match pattern {
            Pattern::Literal(value) => Ok(Self::Literal(decode(value)?)),
            Pattern::Glob(value) => {
                let bytes = decode(value)?;
                let parts = bytes.split(|&byte| byte == b'/');
                let mut components = Vec::new();
                for part in parts {
                    if matches!(part, b"" | b"." | b"..")
                        || (part != b"**" && part.windows(2).any(|bytes| bytes == b"**"))
                    {
                        return Err(Error::InvalidArgument);
                    }
                    components.push(part.to_vec());
                }
                Ok(Self::Glob(components))
            }
        }
    }

    pub fn matches(&self, path: &[u8]) -> bool {
        match self {
            Self::Literal(literal) => path.windows(literal.len()).any(|bytes| bytes == literal),
            Self::Glob(pattern) => {
                // One row per pattern prefix. A double star can consume this
                // component or match zero components after the prior prefix.
                let mut row = vec![false; pattern.len() + 1];
                row[0] = true;
                for (index, part) in pattern.iter().enumerate() {
                    row[index + 1] = row[index] && part == b"**";
                }
                for component in path.split(|&byte| byte == b'/') {
                    let mut diagonal = std::mem::replace(&mut row[0], false);
                    for (index, part) in pattern.iter().enumerate() {
                        let old = row[index + 1];
                        row[index + 1] = if part == b"**" {
                            old || row[index]
                        } else {
                            diagonal && component_matches(part, component)
                        };
                        diagonal = old;
                    }
                }
                row[pattern.len()]
            }
        }
    }
}

fn component_matches(pattern: &[u8], bytes: &[u8]) -> bool {
    let (mut p, mut b) = (0, 0);
    let mut star = None;
    while b < bytes.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p + 1, b));
                p += 1;
            }
            Some(&needle) if needle == b'?' || needle == bytes[b] => {
                p += 1;
                b += 1;
            }
            _ => match star.as_mut() {
                Some((resume, matched)) => {
                    *matched += 1;
                    p = *resume;
                    b = *matched;
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&byte| byte == b'*')
}

#[cfg(test)]
mod tests {
    use super::*;
    use gh_docs_core::git::ByteString;

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
