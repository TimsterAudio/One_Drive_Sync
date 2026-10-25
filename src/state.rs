//! What a pair remembers between runs: the last-synced version of every
//! path, and the remote tree as the delta queries have described it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::remote::{RemoteChange, RemoteItem};

/// A path as it was when both sides last agreed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Synced {
    /// Content hash (QuickXorHash), the same on both sides.
    pub hash: String,
    pub local_size: u64,
    pub local_mtime_ms: i64,
    pub remote_id: String,
    pub remote_etag: String,
}

/// A remote file or folder, kept by id so changes that give only an id
/// (deletions) or only a parent (moves) can be placed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub parent_id: Option<String>,
    pub name: String,
    pub is_folder: bool,
    #[serde(default)]
    pub etag: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub mtime_ms: i64,
    #[serde(default)]
    pub hash: Option<String>,
    /// Where the file was last placed, for when its parents aren't known yet
    /// (a folder an upload has just created).
    #[serde(default)]
    pub path: Option<String>,
}

/// The remote tree under a pair's folder.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTree {
    pub root_id: String,
    pub cursor: Option<String>,
    pub nodes: HashMap<String, Node>,
}

/// A remote file at a path, as the engine sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteFile {
    pub id: String,
    pub etag: String,
    pub size: u64,
    pub mtime_ms: i64,
    pub hash: Option<String>,
}

impl RemoteTree {
    /// Forgets everything, for a full resync.
    pub fn reset(&mut self) {
        self.cursor = None;
        self.nodes.clear();
    }

    pub fn apply(&mut self, changes: Vec<RemoteChange>) {
        let mut deleted = HashSet::new();
        for change in changes {
            match change {
                RemoteChange::Upsert(item) => {
                    if item.id == self.root_id {
                        continue;
                    }
                    deleted.remove(&item.id);
                    let path = self.nodes.get(&item.id).and_then(|n| n.path.clone());
                    self.nodes.insert(item.id.clone(), node_from(item, path));
                }
                RemoteChange::Deleted { id } => {
                    deleted.insert(id);
                }
            }
        }
        if deleted.is_empty() {
            return;
        }
        // A deleted folder takes everything below it.
        let gone: Vec<String> = self
            .nodes
            .keys()
            .filter(|id| self.ancestors(id).any(|a| deleted.contains(a)))
            .cloned()
            .collect();
        for id in gone.iter().chain(deleted.iter()) {
            self.nodes.remove(id);
        }
    }

    /// Records a file this run has just uploaded.
    pub fn record_upload(&mut self, path: &str, item: RemoteItem) {
        self.nodes
            .insert(item.id.clone(), node_from(item, Some(path.to_string())));
    }

    /// The id of the folder at `path` ("" is the root), if known.
    pub fn folder_id(&self, path: &str) -> Option<String> {
        if path.is_empty() {
            return Some(self.root_id.clone());
        }
        self.nodes
            .iter()
            .filter(|(_, n)| n.is_folder)
            .find(|(id, _)| self.resolve(id).as_deref() == Some(path))
            .map(|(id, _)| id.clone())
    }

    /// Records a folder this run has made or looked up.
    pub fn record_folder(&mut self, item: RemoteItem) {
        self.nodes.insert(item.id.clone(), node_from(item, None));
    }

    pub fn forget(&mut self, id: &str) {
        self.nodes.remove(id);
    }

    /// The item and its ancestors' ids, nearest first.
    fn ancestors<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        let mut next = Some(id);
        let mut steps = 0;
        std::iter::from_fn(move || {
            let id = next?;
            steps += 1;
            next = if steps > 256 {
                None
            } else {
                self.nodes.get(id).and_then(|n| n.parent_id.as_deref())
            };
            Some(id)
        })
    }

    /// The path of a node relative to the root, if its parents lead there.
    fn resolve(&self, id: &str) -> Option<String> {
        let mut parts = Vec::new();
        for a in self.ancestors(id) {
            if a == self.root_id {
                parts.reverse();
                return Some(parts.join("/"));
            }
            parts.push(self.nodes.get(a)?.name.as_str());
        }
        None
    }

    /// Every remote file, by path.
    pub fn files(&self) -> BTreeMap<String, RemoteFile> {
        let mut out = BTreeMap::new();
        for (id, n) in &self.nodes {
            if n.is_folder {
                continue;
            }
            let Some(path) = self.resolve(id).or_else(|| n.path.clone()) else {
                continue;
            };
            out.insert(
                path,
                RemoteFile {
                    id: id.clone(),
                    etag: n.etag.clone(),
                    size: n.size,
                    mtime_ms: n.mtime_ms,
                    hash: n.hash.clone(),
                },
            );
        }
        out
    }
}

fn node_from(item: RemoteItem, path: Option<String>) -> Node {
    Node {
        parent_id: item.parent_id,
        name: item.name,
        is_folder: item.is_folder,
        etag: item.etag,
        size: item.size,
        mtime_ms: item.mtime_ms,
        hash: item.hash,
        path,
    }
}

/// Everything a pair saves between runs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PairState {
    /// Which local and remote folders this state belongs to; a state saved
    /// for other folders is not reused.
    pub local: String,
    pub remote: String,
    pub synced: BTreeMap<String, Synced>,
    pub tree: RemoteTree,
}

impl PairState {
    /// Loads the state, or an empty one if there isn't any yet.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Saves the state, replacing the old file only once the new one is
    /// written in full.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, parent: &str, name: &str, folder: bool) -> RemoteItem {
        RemoteItem {
            id: id.into(),
            parent_id: Some(parent.into()),
            name: name.into(),
            is_folder: folder,
            etag: format!("e-{id}"),
            size: 1,
            mtime_ms: 0,
            hash: None,
        }
    }

    #[test]
    fn paths_moves_and_folder_deletes() {
        let mut t = RemoteTree {
            root_id: "root".into(),
            ..Default::default()
        };
        t.apply(vec![
            RemoteChange::Upsert(item("a", "root", "trips", true)),
            RemoteChange::Upsert(item("f1", "a", "x.trip.json", false)),
            RemoteChange::Upsert(item("f2", "root", "top.html", false)),
        ]);
        let files = t.files();
        assert!(files.contains_key("trips/x.trip.json"));
        assert!(files.contains_key("top.html"));

        // Renaming the folder moves its files without listing them.
        t.apply(vec![RemoteChange::Upsert(item("a", "root", "old", true))]);
        assert!(t.files().contains_key("old/x.trip.json"));

        // Deleting the folder takes its files.
        t.apply(vec![RemoteChange::Deleted { id: "a".into() }]);
        let files = t.files();
        assert_eq!(files.len(), 1);
        assert!(files.contains_key("top.html"));
    }
}
