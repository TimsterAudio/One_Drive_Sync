//! An in-memory drive with OneDrive's behaviour where the engine depends on
//! it: eTags, conditional uploads, recycle-bin deletes, and delta queries that
//! report a deleted folder without its children. Clones share one drive, so a
//! test can play another computer editing it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::quickxor;
use crate::remote::{
    Delta, Precondition, RemoteChange, RemoteDrive, RemoteError, RemoteItem, RemoteResult,
    SIMPLE_UPLOAD_LIMIT,
};

#[derive(Clone, Debug)]
struct FakeItem {
    parent: Option<String>,
    name: String,
    folder: bool,
    data: Vec<u8>,
    version: u64,
    mtime_ms: i64,
    deleted: bool,
    /// The change number of the item's last change, for delta queries.
    changed_at: u64,
}

#[derive(Debug, Default)]
struct Inner {
    items: BTreeMap<String, FakeItem>,
    next_id: u64,
    seq: u64,
    clock_ms: i64,
    offline: bool,
    force_resync: bool,
    /// Recycle bin: (path, contents) of deleted files.
    recycled: Vec<(String, Vec<u8>)>,
}

/// A fake OneDrive. `Clone` gives another handle on the same drive.
#[derive(Clone, Debug)]
pub struct FakeDrive {
    inner: Arc<Mutex<Inner>>,
}

impl Default for FakeDrive {
    fn default() -> Self {
        Self::new()
    }
}

const ROOT: &str = "root";

impl FakeDrive {
    pub fn new() -> Self {
        let mut inner = Inner {
            clock_ms: 1_700_000_000_000,
            ..Default::default()
        };
        inner.items.insert(
            ROOT.into(),
            FakeItem {
                parent: None,
                name: String::new(),
                folder: true,
                data: Vec::new(),
                version: 1,
                mtime_ms: inner.clock_ms,
                deleted: false,
                changed_at: 0,
            },
        );
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// While offline, every call fails with [`RemoteError::Offline`].
    pub fn set_offline(&self, offline: bool) {
        self.inner.lock().unwrap().offline = offline;
    }

    /// The next delta query with a cursor fails with
    /// [`RemoteError::ResyncRequired`], as OneDrive does now and then.
    pub fn force_resync(&self) {
        self.inner.lock().unwrap().force_resync = true;
    }

    /// Moves the drive's clock on; remote changes are stamped with it.
    pub fn advance_ms(&self, ms: i64) {
        self.inner.lock().unwrap().clock_ms += ms;
    }

    pub fn now_ms(&self) -> i64 {
        self.inner.lock().unwrap().clock_ms
    }

    /// Writes a file as another computer would, stamped `mtime_ms` if given.
    pub fn put(&self, path: &str, data: &[u8], mtime_ms: Option<i64>) {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.put(path, data);
        if let Some(t) = mtime_ms {
            inner.items.get_mut(&id).unwrap().mtime_ms = t;
        }
    }

    /// Deletes a file or folder as another computer would.
    pub fn remove(&self, path: &str) {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.find(path).expect("no such remote path");
        inner.delete(&id);
    }

    /// Renames or moves a file or folder.
    pub fn rename(&self, from: &str, to: &str) {
        let mut inner = self.inner.lock().unwrap();
        let id = inner.find(from).expect("no such remote path");
        let (dir, name) = split(to);
        let parent = inner.mkdirs(dir);
        inner.seq += 1;
        let seq = inner.seq;
        let it = inner.items.get_mut(&id).unwrap();
        it.parent = Some(parent);
        it.name = name.to_string();
        it.version += 1;
        it.changed_at = seq;
    }

    pub fn read(&self, path: &str) -> Option<Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        let id = inner.find(path)?;
        Some(inner.items[&id].data.clone())
    }

    /// Every file on the drive, by path.
    pub fn files(&self) -> BTreeMap<String, Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        inner
            .items
            .iter()
            .filter(|(_, it)| !it.folder && inner.live(it))
            .filter_map(|(id, it)| Some((inner.path_of(id)?, it.data.clone())))
            .collect()
    }

    /// Paths of the files in the recycle bin.
    pub fn recycled(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        inner.recycled.iter().map(|(p, _)| p.clone()).collect()
    }
}

fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

impl Inner {
    fn live(&self, it: &FakeItem) -> bool {
        let mut cur = Some(it);
        while let Some(c) = cur {
            if c.deleted {
                return false;
            }
            cur = c.parent.as_ref().map(|p| &self.items[p]);
        }
        true
    }

    fn child(&self, parent: &str, name: &str) -> Option<String> {
        self.items
            .iter()
            .find(|(_, it)| {
                !it.deleted
                    && it.parent.as_deref() == Some(parent)
                    && it.name.eq_ignore_ascii_case(name)
            })
            .map(|(id, _)| id.clone())
    }

    fn find(&self, path: &str) -> Option<String> {
        let mut cur = ROOT.to_string();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = self.child(&cur, part)?;
        }
        Some(cur)
    }

    fn path_of(&self, id: &str) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = id;
        while cur != ROOT {
            let it = self.items.get(cur)?;
            parts.push(it.name.clone());
            cur = it.parent.as_deref()?;
        }
        parts.reverse();
        Some(parts.join("/"))
    }

    fn new_id(&mut self) -> String {
        self.next_id += 1;
        format!("ID{:04}", self.next_id)
    }

    fn mkdirs(&mut self, path: &str) -> String {
        let mut cur = ROOT.to_string();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            cur = match self.child(&cur, part) {
                Some(id) => id,
                None => {
                    let id = self.new_id();
                    self.seq += 1;
                    let item = FakeItem {
                        parent: Some(cur),
                        name: part.to_string(),
                        folder: true,
                        data: Vec::new(),
                        version: 1,
                        mtime_ms: self.clock_ms,
                        deleted: false,
                        changed_at: self.seq,
                    };
                    self.items.insert(id.clone(), item);
                    id
                }
            };
        }
        cur
    }

    fn put(&mut self, path: &str, data: &[u8]) -> String {
        let (dir, name) = split(path);
        let parent = self.mkdirs(dir);
        self.seq += 1;
        let (seq, now) = (self.seq, self.clock_ms);
        match self.child(&parent, name) {
            Some(id) => {
                let it = self.items.get_mut(&id).unwrap();
                it.data = data.to_vec();
                it.version += 1;
                it.mtime_ms = now;
                it.changed_at = seq;
                id
            }
            None => {
                let id = self.new_id();
                let item = FakeItem {
                    parent: Some(parent),
                    name: name.to_string(),
                    folder: false,
                    data: data.to_vec(),
                    version: 1,
                    mtime_ms: now,
                    deleted: false,
                    changed_at: seq,
                };
                self.items.insert(id.clone(), item);
                id
            }
        }
    }

    fn delete(&mut self, id: &str) {
        if let Some(path) = self.path_of(id) {
            let prefix = format!("{path}/");
            let mut binned: Vec<(String, Vec<u8>)> = self
                .items
                .iter()
                .filter(|(_, it)| !it.folder && self.live(it))
                .filter_map(|(i, it)| Some((self.path_of(i)?, it.data.clone())))
                .filter(|(p, _)| *p == path || p.starts_with(&prefix))
                .collect();
            self.recycled.append(&mut binned);
        }
        self.seq += 1;
        let seq = self.seq;
        let it = self.items.get_mut(id).unwrap();
        it.deleted = true;
        it.changed_at = seq;
    }

    fn etag(&self, id: &str) -> String {
        format!("\"{{{id}}},{}\"", self.items[id].version)
    }

    fn to_item(&self, id: &str) -> RemoteItem {
        let it = &self.items[id];
        RemoteItem {
            id: id.to_string(),
            parent_id: it.parent.clone(),
            name: it.name.clone(),
            is_folder: it.folder,
            etag: self.etag(id),
            size: it.data.len() as u64,
            mtime_ms: it.mtime_ms,
            hash: (!it.folder).then(|| quickxor::hash(&it.data)),
        }
    }

    fn under(&self, id: &str, folder: &str) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == folder {
                return true;
            }
            cur = self.items.get(c).and_then(|it| it.parent.as_deref());
        }
        false
    }

    fn check(&self) -> RemoteResult<()> {
        if self.offline {
            Err(RemoteError::Offline("fake drive is offline".into()))
        } else {
            Ok(())
        }
    }
}

impl RemoteDrive for FakeDrive {
    fn ensure_folder(&mut self, path: &str) -> RemoteResult<RemoteItem> {
        let mut inner = self.inner.lock().unwrap();
        inner.check()?;
        let id = inner.mkdirs(path);
        Ok(inner.to_item(&id))
    }

    fn delta(&mut self, folder_id: &str, cursor: Option<&str>) -> RemoteResult<Delta> {
        let mut inner = self.inner.lock().unwrap();
        inner.check()?;
        if cursor.is_some() && std::mem::take(&mut inner.force_resync) {
            return Err(RemoteError::ResyncRequired);
        }
        let since: Option<u64> = match cursor {
            Some(c) => Some(c.parse().map_err(|_| RemoteError::ResyncRequired)?),
            None => None,
        };
        let mut changes = Vec::new();
        for (id, it) in &inner.items {
            if !inner.under(id, folder_id) {
                continue;
            }
            match since {
                // A fresh query lists what is there now.
                None => {
                    if inner.live(it) {
                        changes.push(RemoteChange::Upsert(inner.to_item(id)));
                    }
                }
                Some(s) if it.changed_at > s => {
                    if it.deleted {
                        changes.push(RemoteChange::Deleted { id: id.clone() });
                    } else if inner.live(it) {
                        changes.push(RemoteChange::Upsert(inner.to_item(id)));
                    }
                    // Children of a deleted folder aren't listed, as on OneDrive.
                }
                Some(_) => {}
            }
        }
        Ok(Delta {
            changes,
            cursor: inner.seq.to_string(),
            complete: false,
        })
    }

    fn download(&mut self, id: &str) -> RemoteResult<Vec<u8>> {
        let inner = self.inner.lock().unwrap();
        inner.check()?;
        match inner.items.get(id) {
            Some(it) if inner.live(it) && !it.folder => Ok(it.data.clone()),
            _ => Err(RemoteError::NotFound),
        }
    }

    fn upload(&mut self, path: &str, data: &[u8], cond: Precondition) -> RemoteResult<RemoteItem> {
        let mut inner = self.inner.lock().unwrap();
        inner.check()?;
        if data.len() as u64 > SIMPLE_UPLOAD_LIMIT {
            return Err(RemoteError::TooLarge(data.len() as u64));
        }
        let existing = inner.find(path);
        match (&cond, &existing) {
            (Precondition::New, Some(_)) => return Err(RemoteError::Conflict),
            (Precondition::Match(_), None) => return Err(RemoteError::Conflict),
            (Precondition::Match(tag), Some(id)) if *tag != inner.etag(id) => {
                return Err(RemoteError::Conflict)
            }
            _ => {}
        }
        let id = inner.put(path, data);
        Ok(inner.to_item(&id))
    }

    fn delete(&mut self, id: &str, etag: Option<&str>) -> RemoteResult<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.check()?;
        match inner.items.get(id) {
            Some(it) if inner.live(it) => {}
            _ => return Err(RemoteError::NotFound),
        }
        if let Some(tag) = etag {
            if tag != inner.etag(id) {
                return Err(RemoteError::Conflict);
            }
        }
        inner.delete(id);
        Ok(())
    }
}
