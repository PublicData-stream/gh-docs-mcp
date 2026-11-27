//! Traverse authorized commit trees with literal byte paths.

use std::{collections::HashMap, rc::Rc};

use docs_mcp_core::{
    exploration::Policy,
    git::{BytePath, Oid},
};
use docs_mcp_git::reader::{Batch, TreeEntry, TreeMode};
use serde_json::json;

use super::{Error, matcher::Matcher, page::Entries};

const CACHE_BYTES: usize = 2 * 1024 * 1024;
const NAVIGATION_CACHE_BYTES: usize = 512 * 1024;
pub(super) const MAX_DEPTH: u8 = 64;

pub(super) struct Tree<'a, 'reader> {
    batch: &'a mut Batch<'reader>,
    policy: &'a Policy,
    cache: HashMap<Oid, Rc<Vec<TreeEntry>>>,
    cached_bytes: usize,
    navigation_cache: HashMap<(Oid, Vec<u8>), bool>,
    navigation_bytes: usize,
}

impl<'a, 'reader> Tree<'a, 'reader> {
    pub fn new(batch: &'a mut Batch<'reader>, policy: &'a Policy) -> Self {
        Self {
            batch,
            policy,
            cache: HashMap::new(),
            cached_bytes: 0,
            navigation_cache: HashMap::new(),
            navigation_bytes: 0,
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

    pub fn notice(&self, path: &[u8], mode: &TreeMode) -> Option<&'a str> {
        self.policy
            .scope
            .direct_notice(path, *mode == TreeMode::Directory)
            .or_else(|| self.glob_notice(path, mode))
    }

    fn glob_notice(&self, path: &[u8], mode: &TreeMode) -> Option<&'a str> {
        match mode {
            TreeMode::Regular | TreeMode::Executable => self.policy.scope.glob_notice(path),
            _ => None,
        }
    }

    pub fn approved_leaf(
        &mut self,
        root: Oid,
        path: &[u8],
    ) -> Result<Option<(TreeEntry, &'a str)>, Error> {
        if !self.policy.scope.allows_leaf_path(path) {
            return Ok(None);
        }
        Ok(self
            .lookup(root, path)?
            .filter(|entry| entry.mode != TreeMode::Directory)
            .and_then(|entry| self.notice(path, &entry.mode).map(|notice| (entry, notice))))
    }

    pub fn blob(&mut self, oid: Oid) -> Result<Vec<u8>, Error> {
        Ok(self
            .batch
            .contents(oid, docs_mcp_core::git::ObjectType::Blob)?)
    }

    fn literal_navigation(&mut self, oid: Oid, path: &[u8]) -> Result<bool, Error> {
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

    fn glob_navigation(&mut self, oid: Oid, path: &[u8]) -> Result<bool, Error> {
        if !self.policy.scope.can_match_glob_below(path) {
            return Ok(false);
        }
        let key = (oid, path.to_vec());
        if let Some(&approved) = self.navigation_cache.get(&key) {
            self.batch.charge_visits(1)?;
            return Ok(approved);
        }
        if path.split(|&b| b == b'/').count() >= MAX_DEPTH.into() {
            return Err(Error::Budget);
        }
        let mut child_path = path.to_vec();
        child_path.push(b'/');
        let prefix_len = child_path.len();
        let mut approved = false;
        for entry in self.children(oid)?.iter() {
            self.batch.check()?;
            child_path.truncate(prefix_len);
            child_path.extend_from_slice(&entry.name);
            if self.glob_notice(&child_path, &entry.mode).is_some()
                || (entry.mode == TreeMode::Directory
                    && self.glob_navigation(entry.oid, &child_path)?)
            {
                approved = true;
                break;
            }
        }
        let size = path.len() + 128;
        if self.navigation_bytes + size > NAVIGATION_CACHE_BYTES {
            self.navigation_cache.clear();
            self.navigation_bytes = 0;
        }
        if size <= NAVIGATION_CACHE_BYTES {
            self.navigation_cache.insert(key, approved);
            self.navigation_bytes += size;
        }
        Ok(approved)
    }

    pub fn directory(&mut self, commit_tree: Oid, path: &[u8]) -> Result<Oid, Error> {
        if path.is_empty() {
            return Ok(commit_tree);
        }
        let approved = self.policy.scope.direct_notice(path, true).is_some();
        if !approved
            && self.policy.scope.descendants(path).next().is_none()
            && !self.policy.scope.can_match_glob_below(path)
        {
            return Err(Error::ObjectUnavailable);
        }
        let entry = self
            .lookup(commit_tree, path)?
            .filter(|entry| entry.mode == TreeMode::Directory)
            .ok_or(Error::ObjectUnavailable)?;
        if approved
            || self.literal_navigation(entry.oid, path)?
            || self.glob_navigation(entry.oid, path)?
        {
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
        leaves_only: bool,
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
            let descend = depth > 1 && entry.mode == TreeMode::Directory;
            if !leaves_only || !descend {
                actions.push((index, false));
            }
            if descend {
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
            let notice = self.notice(&path, &entry.mode);
            let navigation = notice.is_none()
                && directory
                && (self.literal_navigation(entry.oid, &path)?
                    || if leaves_only && descend {
                        // A leaf traversal can check entries as it goes, without
                        // proving that a directory would be visible on its own.
                        self.policy.scope.can_match_glob_below(&path)
                    } else {
                        self.glob_navigation(entry.oid, &path)?
                    });
            if notice.is_none() && !navigation {
                continue;
            }
            if descend {
                if !self.visit(entry.oid, &path, depth - 1, leaves_only, visit)? {
                    return Ok(false);
                }
            } else {
                if directory && leaves_only {
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
