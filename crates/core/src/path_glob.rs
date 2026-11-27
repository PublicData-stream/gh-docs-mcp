//! Anchored glob matching on raw repository-relative Git paths.

use crate::git::validate_component;

pub const MAX_GLOB_BYTES: usize = 256;

#[derive(Debug)]
pub struct PathGlob(Vec<Vec<u8>>);

impl PathGlob {
    pub fn compile(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() > MAX_GLOB_BYTES {
            return Err("path glob exceeds byte limit");
        }
        let mut components = Vec::new();
        for part in bytes.split(|&byte| byte == b'/') {
            validate_component(part)?;
            if part != b"**" && part.windows(2).any(|bytes| bytes == b"**") {
                return Err("double star must be a whole glob component");
            }
            components.push(part.to_vec());
        }
        Ok(Self(components))
    }

    pub fn matches(&self, path: &[u8]) -> bool {
        self.prefix_states(path)[self.0.len()]
    }

    pub fn can_match_descendant(&self, directory: &[u8]) -> bool {
        // A remaining component can consume at least one more component.
        // A trailing ** can also consume another after a full match.
        let row = self.prefix_states(directory);
        row[..self.0.len()].iter().any(|&active| active)
            || (row[self.0.len()] && self.0.last().is_some_and(|part| part == b"**"))
    }

    fn prefix_states(&self, path: &[u8]) -> [bool; MAX_GLOB_BYTES + 1] {
        let mut row = [false; MAX_GLOB_BYTES + 1];
        row[0] = true;
        for (index, part) in self.0.iter().enumerate() {
            row[index + 1] = row[index] && part == b"**";
        }
        if path.is_empty() {
            return row;
        }
        for component in path.split(|&byte| byte == b'/') {
            let mut diagonal = std::mem::replace(&mut row[0], false);
            for (index, part) in self.0.iter().enumerate() {
                let prior = row[index + 1];
                row[index + 1] = if part == b"**" {
                    prior || row[index]
                } else {
                    diagonal && component_matches(part, component)
                };
                diagonal = prior;
            }
            if !row[..=self.0.len()].iter().any(|&active| active) {
                break;
            }
        }
        row
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
    use super::PathGlob;

    #[test]
    fn descendant_matching_handles_remaining_and_recursive_components() {
        let fixed = PathGlob::compile(b"docs/*/guide.md").unwrap();
        assert!(fixed.can_match_descendant(b"docs/setup"));
        assert!(!fixed.can_match_descendant(b"docs/setup/guide.md"));
        assert!(!fixed.can_match_descendant(b"docs/setup/nested"));
        assert!(!fixed.can_match_descendant(b"docs-backup/setup"));

        let recursive = PathGlob::compile(b"docs/**").unwrap();
        assert!(recursive.matches(b"docs"));
        assert!(recursive.can_match_descendant(b"docs"));
        assert!(recursive.can_match_descendant(b"docs/setup/\xff"));
        assert!(!recursive.can_match_descendant(b"src/docs"));
    }
}
