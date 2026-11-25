//! Traverse authorized commit trees with literal byte paths.

use std::{collections::HashMap, rc::Rc};

use gh_docs_core::{
    exploration::Policy,
    git::{BytePath, Oid},
};
use gh_docs_git::reader::{Batch, TreeEntry, TreeMode};
use serde_json::json;

use super::{Error, matcher::Matcher, page::Entries};

const CACHE_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_DEPTH: u8 = 64;

pub(super) struct Tree<'a, 'reader> {
    batch: &'a mut Batch<'reader>,
    policy: &'a Policy,
    cache: HashMap<Oid, Rc<Vec<TreeEntry>>>,
    cached_bytes: usize,
}

impl<'a, 'reader> Tree<'a, 'reader> {
    pub fn new(batch: &'a mut Batch<'reader>, policy: &'a Policy) -> Self {
        Self {
            batch,
            policy,
            cache: HashMap::new(),
            cached_bytes: 0,
        }
    }

    fn children(&mut self, oid: Oid) -> Result<Rc<Vec<TreeEntry>>, Error> {
        self.batch.check()?;
        if let Some(entries) = self.cache.get(&oid) {
            self.batch.charge_visits(entries.len() + 1)?;
            return Ok(Rc::clone(entries));
        }
        let mut entries = self.batch.tree(oid)?;
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        let size = entries
            .iter()
            .map(|entry| entry.name.len() + 128)
            .sum::<usize>();
        let entries = Rc::new(entries);
        if size <= CACHE_BYTES {
            if self.cached_bytes + size > CACHE_BYTES {
                self.cache.clear();
                self.cached_bytes = 0;
            }
            self.cache.insert(oid, Rc::clone(&entries));
            self.cached_bytes += size;
        }
        Ok(entries)
    }

    pub fn lookup(&mut self, mut tree: Oid, path: &[u8]) -> Result<Option<TreeEntry>, Error> {
        let mut components = path.split(|&byte| byte == b'/').peekable();
        for depth in 0..MAX_DEPTH {
            let Some(component) = components.next() else {
                return Ok(None);
            };
            let entries = self.children(tree)?;
            let Ok(index) = entries.binary_search_by(|entry| entry.name.as_slice().cmp(component))
            else {
                return Ok(None);
            };
            let entry = &entries[index];
            if components.peek().is_none() {
                return Ok(Some(entry.clone()));
            }
            if entry.mode != TreeMode::Directory {
                return Ok(None);
            }
            if depth + 1 == MAX_DEPTH {
                return Err(Error::Budget);
            }
            tree = entry.oid;
        }
        Ok(None)
    }

    pub fn leaf(&mut self, root: Oid, path: &[u8]) -> Result<(TreeEntry, &str), Error> {
        let notice = self
            .policy
            .scope
            .notice(path, false)
            .ok_or(Error::ObjectUnavailable)?;
        let leaf = self
            .lookup(root, path)?
            .filter(|entry| entry.mode != TreeMode::Directory)
            .ok_or(Error::ObjectUnavailable)?;
        Ok((leaf, notice))
    }

    pub fn blob(&mut self, oid: Oid) -> Result<Vec<u8>, Error> {
        Ok(self
            .batch
            .contents(oid, gh_docs_core::git::ObjectType::Blob)?)
    }

    fn navigation(&mut self, oid: Oid, path: &[u8]) -> Result<bool, Error> {
        for grant in self.policy.scope.descendants(path) {
            let relative = &grant.path[path.len() + 1..];
            if self
                .lookup(oid, relative)?
                .is_some_and(|entry| (entry.mode == TreeMode::Directory) == grant.subtree)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn directory(&mut self, commit_tree: Oid, path: &[u8]) -> Result<Oid, Error> {
        if path.is_empty() {
            return Ok(commit_tree);
        }
        let approved = self.policy.scope.notice(path, true).is_some();
        if !approved && self.policy.scope.descendants(path).next().is_none() {
            return Err(Error::ObjectUnavailable);
        }
        let entry = self
            .lookup(commit_tree, path)?
            .filter(|entry| entry.mode == TreeMode::Directory)
            .ok_or(Error::ObjectUnavailable)?;
        if approved || self.navigation(entry.oid, path)? {
            Ok(entry.oid)
        } else {
            Err(Error::ObjectUnavailable)
        }
    }

    pub fn batch(&mut self) -> &mut Batch<'reader> {
        self.batch
    }

    pub fn walk(
        &mut self,
        tree: Oid,
        root: &[u8],
        depth: u8,
        matcher: Option<&Matcher>,
        output: &mut Entries,
    ) -> Result<bool, Error> {
        let metadata_notice = &self.policy.metadata_notice;
        self.visit(tree, root, depth, false, &mut |_, entry, path, notice| {
            if matcher.is_some_and(|matcher| !matcher.matches(path)) {
                return Ok(true);
            }
            let path = BytePath::encode(path).map_err(|_| Error::UnsupportedEncoding)?;
            let (mode, kind) = mode(&entry.mode);
            let mut item = json!({
                "path":path, "mode":mode, "kind":kind, "navigation_only":notice.is_none(),
                "notice":notice.unwrap_or(metadata_notice),
            });
            if notice.is_some() {
                item["oid"] = json!(entry.oid.to_string());
            }
            output.push(item)
        })
    }

    pub fn visit(
        &mut self,
        tree: Oid,
        root: &[u8],
        depth: u8,
        require_full_depth: bool,
        visit: &mut impl FnMut(
            &mut Batch<'reader>,
            &TreeEntry,
            &[u8],
            Option<&str>,
        ) -> Result<bool, Error>,
    ) -> Result<bool, Error> {
        let children = self.children(tree)?;
        let mut actions = Vec::new();
        for (index, entry) in children.iter().enumerate() {
            actions.push((index, false));
            if depth > 1 && entry.mode == TreeMode::Directory {
                actions.push((index, true));
            }
        }
        // A directory's entry sorts as "name", and its descendants sort
        // as "name/…". Siblings like "name-" can fall between the two.
        let key = |(index, descend): (usize, bool)| {
            children[index]
                .name
                .iter()
                .copied()
                .chain(descend.then_some(b'/'))
        };
        actions.sort_unstable_by(|&left, &right| key(left).cmp(key(right)));
        for (index, descend) in actions {
            self.batch.check()?;
            let entry = &children[index];
            let mut path = Vec::with_capacity(root.len() + 1 + entry.name.len());
            path.extend_from_slice(root);
            if !root.is_empty() {
                path.push(b'/');
            }
            path.extend_from_slice(&entry.name);
            let directory = entry.mode == TreeMode::Directory;
            let notice = self.policy.scope.notice(&path, directory);
            let navigation = notice.is_none() && directory && self.navigation(entry.oid, &path)?;
            if notice.is_none() && !navigation {
                continue;
            }
            if descend {
                if !self.visit(entry.oid, &path, depth - 1, require_full_depth, visit)? {
                    return Ok(false);
                }
            } else {
                if directory && depth == 1 && require_full_depth {
                    return Err(Error::Budget);
                }
                if !visit(self.batch, entry, &path, notice)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

pub(super) fn mode(mode: &TreeMode) -> (&'static str, &'static str) {
    match mode {
        TreeMode::Directory => ("040000", "tree"),
        TreeMode::Regular => ("100644", "blob"),
        TreeMode::Executable => ("100755", "blob"),
        TreeMode::Symlink => ("120000", "symlink"),
        TreeMode::Gitlink => ("160000", "gitlink"),
    }
}
