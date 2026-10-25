//! [`RemoteDrive`] over a plain folder: a LAN share mounted on this computer
//! (SMB/CIFS), a USB drive, or another local folder.
//!
//! Item ids are paths relative to the folder, and an eTag is the file's size
//! and modification time. There is no change journal, so every delta query
//! lists the whole folder. Hashes are kept in memory against size and time,
//! so a file is read again only once it has changed.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::local::{self, TMP_SUFFIX};
use crate::remote::{
    Delta, Precondition, RemoteChange, RemoteDrive, RemoteError, RemoteItem, RemoteResult,
};

/// Where deleted files go, inside the shared folder, so a deletion that
/// comes over from the other side can be undone.
pub const TRASH_DIR: &str = ".odsync-trash";

/// A folder standing in for the remote side.
pub struct FolderDrive {
    root: PathBuf,
    /// path -> (size, mtime in ns, QuickXorHash)
    hashes: HashMap<String, (u64, i128, String)>,
}

fn ns(t: SystemTime) -> i128 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}

fn parent_of(id: &str) -> String {
    id.rsplit_once('/')
        .map(|(p, _)| p.to_string())
        .unwrap_or_default()
}

fn name_of(id: &str) -> &str {
    id.rsplit('/').next().unwrap_or(id)
}

impl FolderDrive {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            hashes: HashMap::new(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, id: &str) -> PathBuf {
        local::full_path(&self.root, id.trim_matches('/'))
    }

    /// Fails as offline if the folder isn't there (the share isn't mounted,
    /// the drive isn't plugged in).
    fn reachable(&self) -> RemoteResult<()> {
        if self.root.is_dir() {
            Ok(())
        } else {
            Err(RemoteError::Offline(format!(
                "{} isn't reachable (is the share mounted?)",
                self.root.display()
            )))
        }
    }

    fn io_error(&self, e: io::Error) -> RemoteError {
        if e.kind() == io::ErrorKind::NotFound {
            if self.root.is_dir() {
                RemoteError::NotFound
            } else {
                RemoteError::Offline(format!("{} went away", self.root.display()))
            }
        } else {
            RemoteError::Other(e.to_string())
        }
    }

    /// The item at `id`, hashing it if it's a file whose hash isn't known.
    fn item(&mut self, id: &str) -> RemoteResult<RemoteItem> {
        let id = id.trim_matches('/').to_string();
        let path = self.path(&id);
        let meta = fs::metadata(&path).map_err(|e| self.io_error(e))?;
        let modified = meta.modified().map_err(|e| self.io_error(e))?;
        let mtime = ns(modified);
        let is_folder = meta.is_dir();
        let hash = if is_folder {
            None
        } else {
            match self.hashes.get(&id) {
                Some((size, t, h)) if *size == meta.len() && *t == mtime => Some(h.clone()),
                _ => {
                    let h = local::hash_file(&path).map_err(|e| self.io_error(e))?;
                    self.hashes
                        .insert(id.clone(), (meta.len(), mtime, h.clone()));
                    Some(h)
                }
            }
        };
        Ok(RemoteItem {
            parent_id: (!id.is_empty()).then(|| parent_of(&id)),
            name: name_of(&id).to_string(),
            is_folder,
            etag: if is_folder {
                String::new()
            } else {
                format!("{}-{mtime}", meta.len())
            },
            size: if is_folder { 0 } else { meta.len() },
            mtime_ms: local::to_ms(modified),
            hash,
            id,
        })
    }

    /// Every file and folder under `id`, the folder itself included.
    fn list(&mut self, id: &str) -> RemoteResult<Vec<RemoteItem>> {
        let mut out = vec![self.item(id)?];
        let mut stack = vec![id.trim_matches('/').to_string()];
        while let Some(dir) = stack.pop() {
            let entries = fs::read_dir(self.path(&dir)).map_err(|e| self.io_error(e))?;
            for entry in entries {
                let entry = entry.map_err(|e| self.io_error(e))?;
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if name.ends_with(TMP_SUFFIX) || (dir.is_empty() && name == TRASH_DIR) {
                    continue;
                }
                let child = if dir.is_empty() {
                    name
                } else {
                    format!("{dir}/{name}")
                };
                let ft = entry.file_type().map_err(|e| self.io_error(e))?;
                if ft.is_symlink() {
                    continue;
                }
                // A file may go between listing and reading; skip it.
                match self.item(&child) {
                    Ok(item) => {
                        if item.is_folder {
                            stack.push(child);
                        }
                        out.push(item);
                    }
                    Err(RemoteError::NotFound) => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(out)
    }

    fn check(&mut self, id: &str, cond: &Precondition) -> RemoteResult<()> {
        let current = match self.item(id) {
            Ok(item) => Some(item),
            Err(RemoteError::NotFound) => None,
            Err(e) => return Err(e),
        };
        match (cond, current) {
            (Precondition::New, Some(_)) => Err(RemoteError::Conflict),
            (Precondition::Match(_), None) => Err(RemoteError::Conflict),
            (Precondition::Match(tag), Some(item)) if *tag != item.etag => {
                Err(RemoteError::Conflict)
            }
            _ => Ok(()),
        }
    }
}

impl RemoteDrive for FolderDrive {
    fn ensure_folder(&mut self, path: &str) -> RemoteResult<RemoteItem> {
        self.reachable()?;
        fs::create_dir_all(self.path(path)).map_err(|e| self.io_error(e))?;
        self.item(path)
    }

    fn delta(&mut self, folder_id: &str, _cursor: Option<&str>) -> RemoteResult<Delta> {
        self.reachable()?;
        let items = self.list(folder_id)?;
        let live: std::collections::HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
        self.hashes.retain(|k, _| live.contains(k.as_str()));
        Ok(Delta {
            changes: items.into_iter().map(RemoteChange::Upsert).collect(),
            cursor: String::new(),
            complete: true,
        })
    }

    fn download(&mut self, id: &str) -> RemoteResult<Vec<u8>> {
        self.reachable()?;
        fs::read(self.path(id)).map_err(|e| self.io_error(e))
    }

    fn upload(&mut self, path: &str, data: &[u8], cond: Precondition) -> RemoteResult<RemoteItem> {
        self.reachable()?;
        self.check(path, &cond)?;
        local::replace(&self.path(path), data).map_err(|e| self.io_error(e))?;
        self.item(path)
    }

    fn delete(&mut self, id: &str, etag: Option<&str>) -> RemoteResult<()> {
        self.reachable()?;
        if let Some(tag) = etag {
            self.check(id, &Precondition::Match(tag.to_string()))
                .map_err(|e| match e {
                    // Already gone counts as deleted.
                    RemoteError::Conflict if !self.path(id).exists() => RemoteError::NotFound,
                    e => e,
                })?;
        }
        let stamp = local::now_ms();
        let to = self.root.join(TRASH_DIR).join(stamp.to_string());
        let to = local::full_path(&to, id.trim_matches('/'));
        if let Some(dir) = to.parent() {
            fs::create_dir_all(dir).map_err(|e| self.io_error(e))?;
        }
        fs::rename(self.path(id), &to).map_err(|e| self.io_error(e))?;
        self.hashes.remove(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_uploads_and_deletes_to_its_trash() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut d = FolderDrive::new(dir.path());
        let root = d.ensure_folder("trips").unwrap();
        assert_eq!(root.id, "trips");

        let a = d
            .upload("trips/a/x.trip.json", b"x", Precondition::New)
            .unwrap();
        assert_eq!(a.parent_id.as_deref(), Some("trips/a"));
        assert!(matches!(
            d.upload("trips/a/x.trip.json", b"y", Precondition::New),
            Err(RemoteError::Conflict)
        ));
        assert!(matches!(
            d.upload(
                "trips/a/x.trip.json",
                b"y",
                Precondition::Match("0-0".into())
            ),
            Err(RemoteError::Conflict)
        ));

        let delta = d.delta("trips", None).unwrap();
        assert!(delta.complete);
        let ids: Vec<String> = delta
            .changes
            .iter()
            .map(|c| match c {
                RemoteChange::Upsert(i) => i.id.clone(),
                RemoteChange::Deleted { id } => id.clone(),
            })
            .collect();
        assert_eq!(ids, ["trips", "trips/a", "trips/a/x.trip.json"]);

        d.delete("trips/a/x.trip.json", Some(&a.etag)).unwrap();
        assert!(!dir.path().join("trips/a/x.trip.json").exists());
        let trash = dir.path().join(TRASH_DIR);
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 1);
    }

    #[test]
    fn a_missing_folder_is_offline() {
        let mut d = FolderDrive::new("/nonexistent/odsync-share");
        assert!(matches!(d.delta("", None), Err(RemoteError::Offline(_))));
    }
}
