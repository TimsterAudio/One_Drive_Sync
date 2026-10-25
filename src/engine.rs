//! The sync engine: compares each side with the state left by the last run
//! and carries changes across. It never looks inside a file.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::local::{self, LocalFile};
use crate::quickxor;
use crate::remote::{Precondition, RemoteDrive, RemoteError, RemoteItem};
use crate::rules::{conflict_name, BothChanged, DeleteVsEdit, Rules};
use crate::state::{PairState, RemoteFile, Synced};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// Local changes go up; OneDrive's copy is replaced, even if it changed.
    Push,
    /// Changes go both ways; the pair's rules settle files changed on both.
    TwoWay,
}

/// A local folder kept in sync with a OneDrive folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairConfig {
    pub local: PathBuf,
    /// The OneDrive folder, relative to the drive's base (the app folder or
    /// the whole drive), `/`-separated.
    pub remote: String,
    pub mode: Mode,
    /// How often to ask OneDrive for changes, in seconds.
    #[serde(default = "default_poll_secs")]
    pub poll_secs: u64,
    #[serde(default)]
    pub rules: Rules,
}

fn default_poll_secs() -> u64 {
    20
}

impl PairConfig {
    pub fn new(local: impl Into<PathBuf>, remote: &str, mode: Mode) -> Self {
        Self {
            local: local.into(),
            remote: remote.trim_matches('/').to_string(),
            mode,
            poll_secs: default_poll_secs(),
            rules: Rules::default(),
        }
    }

    fn remote_path(&self, rel: &str) -> String {
        if self.remote.is_empty() {
            rel.to_string()
        } else {
            format!("{}/{rel}", self.remote)
        }
    }
}

#[derive(Clone, Debug)]
pub struct SyncOptions {
    /// Names this computer in conflict copies.
    pub host: String,
    /// Local deletions go to the system's wastebasket rather than for good.
    pub to_trash: bool,
    /// A file changed this recently (ms) is left for the next run, in case
    /// it is still being written.
    pub settle_ms: i64,
    /// If one side is empty while the other still has files, and at least
    /// this many were synced, stop instead of deleting them all: the folder
    /// is more likely missing than emptied.
    pub guard_min: usize,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            host: host_name(),
            to_trash: true,
            settle_ms: 2000,
            guard_min: 5,
        }
    }
}

/// This computer's name, for conflict copies.
pub fn host_name() -> String {
    let name = gethostname::gethostname().to_string_lossy().into_owned();
    let name: String = name
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    if name.is_empty() {
        "this computer".into()
    } else {
        name
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub level: Level,
    pub message: String,
}

/// What a run did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub uploaded: usize,
    pub downloaded: usize,
    pub deleted_local: usize,
    pub deleted_remote: usize,
    pub conflicts: usize,
    /// Files left for the next run (still being written, or changed meanwhile).
    pub deferred: usize,
    /// Files that failed and will be retried.
    pub failed: usize,
}

impl Report {
    pub fn changed(&self) -> bool {
        self.uploaded + self.downloaded + self.deleted_local + self.deleted_remote + self.conflicts
            > 0
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts = Vec::new();
        for (n, what) in [
            (self.uploaded, "uploaded"),
            (self.downloaded, "downloaded"),
            (self.deleted_local, "deleted here"),
            (self.deleted_remote, "deleted on OneDrive"),
            (self.conflicts, "conflicts kept"),
            (self.deferred, "waiting"),
            (self.failed, "failed"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        if parts.is_empty() {
            write!(f, "up to date")
        } else {
            write!(f, "{}", parts.join(", "))
        }
    }
}

#[derive(Debug)]
pub enum SyncError {
    /// The local folder or the state file couldn't be read or written.
    Local(io::Error),
    /// The run stopped on a remote error that affects every file (offline,
    /// signed out, throttled).
    Remote(RemoteError),
    /// Stopped rather than delete everything on one side.
    Guard(String),
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Local(e) => write!(f, "{e}"),
            SyncError::Remote(e) => write!(f, "{e}"),
            SyncError::Guard(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<io::Error> for SyncError {
    fn from(e: io::Error) -> Self {
        SyncError::Local(e)
    }
}

impl From<RemoteError> for SyncError {
    fn from(e: RemoteError) -> Self {
        SyncError::Remote(e)
    }
}

/// Errors that stop the whole run rather than one file.
fn is_fatal(e: &RemoteError) -> bool {
    matches!(
        e,
        RemoteError::Offline(_)
            | RemoteError::Auth(_)
            | RemoteError::Throttled(_)
            | RemoteError::ResyncRequired
    )
}

/// Runs one sync of `pair`, keeping its state in `state_path`.
pub fn sync(
    pair: &PairConfig,
    opts: &SyncOptions,
    state_path: &Path,
    drive: &mut dyn RemoteDrive,
    log: &mut dyn FnMut(LogLine),
) -> Result<Report, SyncError> {
    if !pair.local.is_dir() {
        return Err(SyncError::Local(io::Error::new(
            io::ErrorKind::NotFound,
            format!("local folder {} is missing", pair.local.display()),
        )));
    }
    let mut state = PairState::load(state_path)?;
    let local_key = pair.local.to_string_lossy().into_owned();
    if state.local != local_key || state.remote != pair.remote {
        state = PairState {
            local: local_key,
            remote: pair.remote.clone(),
            ..Default::default()
        };
    }
    let mut run = Run {
        pair,
        opts,
        state_path,
        drive,
        log,
        state,
        report: Report::default(),
        now_ms: local::now_ms(),
    };
    match pair.mode {
        Mode::Push => run.push()?,
        Mode::TwoWay => run.two_way()?,
    }
    run.save()?;
    Ok(run.report)
}

/// The local side of a path for this run.
#[derive(Clone, Debug)]
struct LocalSide {
    file: Option<LocalFile>,
    /// Content hash, when the file is there.
    hash: Option<String>,
    changed: bool,
}

struct Run<'a> {
    pair: &'a PairConfig,
    opts: &'a SyncOptions,
    state_path: &'a Path,
    drive: &'a mut dyn RemoteDrive,
    log: &'a mut dyn FnMut(LogLine),
    state: PairState,
    report: Report,
    now_ms: i64,
}

impl Run<'_> {
    fn say(&mut self, level: Level, message: String) {
        (self.log)(LogLine { level, message });
    }

    fn save(&self) -> io::Result<()> {
        self.state.save(self.state_path)
    }

    fn full(&self, rel: &str) -> PathBuf {
        local::full_path(&self.pair.local, rel)
    }

    /// Compares the local file with the last sync; hashes only when the size
    /// or time moved. `None` means leave it for now (still being written, or
    /// unreadable).
    fn local_side(&mut self, rel: &str, file: Option<LocalFile>) -> Option<LocalSide> {
        let base = self.state.synced.get(rel);
        let Some(f) = file else {
            return Some(LocalSide {
                file: None,
                hash: None,
                changed: base.is_some(),
            });
        };
        if let Some(b) = base {
            if b.local_size == f.size && b.local_mtime_ms == f.mtime_ms {
                return Some(LocalSide {
                    file,
                    hash: Some(b.hash.clone()),
                    changed: false,
                });
            }
        }
        let hash = match local::hash_file(&self.full(rel)) {
            Ok(h) => h,
            Err(e) => {
                self.say(Level::Warn, format!("{rel}: can't read it: {e}"));
                self.report.failed += 1;
                return None;
            }
        };
        if let Some(b) = self.state.synced.get_mut(rel) {
            if b.hash == hash {
                // Touched, not changed.
                b.local_size = f.size;
                b.local_mtime_ms = f.mtime_ms;
                return Some(LocalSide {
                    file,
                    hash: Some(hash),
                    changed: false,
                });
            }
        }
        // A time in the future (another clock) isn't waited for.
        if (0..self.opts.settle_ms).contains(&(self.now_ms - f.mtime_ms)) {
            self.report.deferred += 1;
            return None;
        }
        Some(LocalSide {
            file,
            hash: Some(hash),
            changed: true,
        })
    }

    /// Stops a run that would empty one side because the other is empty.
    fn guard(&self, local_count: usize, remote_count: usize) -> Result<(), SyncError> {
        let synced = self.state.synced.len();
        if synced < self.opts.guard_min.max(1) {
            return Ok(());
        }
        if local_count == 0 && remote_count > 0 {
            return Err(SyncError::Guard(format!(
                "the local folder {} is empty but {synced} files were synced; not deleting \
                 them from OneDrive. Restore the folder, or empty the OneDrive folder too.",
                self.pair.local.display()
            )));
        }
        if remote_count == 0 && local_count > 0 {
            return Err(SyncError::Guard(format!(
                "the OneDrive folder {} is empty but {synced} files were synced; not deleting \
                 them here. Restore it on OneDrive, or empty the local folder too.",
                self.pair.remote
            )));
        }
        Ok(())
    }

    /// Handles a remote error on one file: stops the run on errors that
    /// affect every file, otherwise logs it and carries on.
    fn remote_failed(&mut self, rel: &str, what: &str, e: RemoteError) -> Result<(), SyncError> {
        if is_fatal(&e) {
            self.save()?;
            return Err(SyncError::Remote(e));
        }
        match e {
            RemoteError::Conflict => {
                self.say(
                    Level::Info,
                    format!("{rel}: changed on OneDrive meanwhile; next run settles it"),
                );
                self.report.deferred += 1;
            }
            e => {
                self.say(Level::Warn, format!("{rel}: {what} failed: {e}"));
                self.report.failed += 1;
            }
        }
        Ok(())
    }

    // ---- Push ----

    fn push(&mut self) -> Result<(), SyncError> {
        let local = local::scan(&self.pair.local, &self.pair.rules)?;
        let paths: BTreeSet<String> = local
            .keys()
            .chain(self.state.synced.keys())
            .cloned()
            .collect();
        if local.is_empty() {
            self.guard(0, self.state.synced.len())?;
        }
        for rel in paths {
            let Some(side) = self.local_side(&rel, local.get(&rel).copied()) else {
                continue;
            };
            if !side.changed {
                continue;
            }
            let base = self.state.synced.get(&rel).cloned();
            match (&side.file, base) {
                (Some(_), base) => {
                    let cond = match &base {
                        Some(b) => Precondition::Match(b.remote_etag.clone()),
                        None => Precondition::New,
                    };
                    self.upload(&rel, &side, cond, true)?;
                }
                (None, Some(b)) => {
                    match self.drive.delete(&b.remote_id, Some(&b.remote_etag)) {
                        Ok(()) | Err(RemoteError::NotFound) => {}
                        Err(RemoteError::Conflict) => {
                            self.say(
                                Level::Warn,
                                format!("{rel}: changed on OneDrive; push mode deletes it anyway"),
                            );
                            match self.drive.delete(&b.remote_id, None) {
                                Ok(()) | Err(RemoteError::NotFound) => {}
                                Err(e) => {
                                    self.remote_failed(&rel, "deleting", e)?;
                                    continue;
                                }
                            }
                        }
                        Err(e) => {
                            self.remote_failed(&rel, "deleting", e)?;
                            continue;
                        }
                    }
                    self.state.synced.remove(&rel);
                    self.report.deleted_remote += 1;
                    self.say(Level::Info, format!("{rel}: deleted on OneDrive"));
                    self.save()?;
                }
                (None, None) => {}
            }
        }
        Ok(())
    }

    // ---- Two-way ----

    /// Brings the remote tree up to date with a delta query.
    fn refresh_remote(&mut self) -> Result<(), SyncError> {
        let root = self.drive.ensure_folder(&self.pair.remote)?;
        let tree = &mut self.state.tree;
        if tree.root_id != root.id {
            tree.reset();
            tree.root_id = root.id.clone();
        }
        let delta = match self.drive.delta(&root.id, tree.cursor.as_deref()) {
            Err(RemoteError::ResyncRequired) => {
                tree.reset();
                self.drive.delta(&root.id, None)?
            }
            r => r?,
        };
        let tree = &mut self.state.tree;
        if tree.cursor.is_none() {
            tree.nodes.clear();
        }
        tree.apply(delta.changes);
        tree.cursor = Some(delta.cursor);
        self.save()?;
        Ok(())
    }

    fn two_way(&mut self) -> Result<(), SyncError> {
        self.refresh_remote()?;
        let rules = &self.pair.rules;
        let remote: BTreeMap<String, RemoteFile> = self
            .state
            .tree
            .files()
            .into_iter()
            .filter(|(p, _)| !rules.is_ignored(p))
            .collect();
        let local = local::scan(&self.pair.local, rules)?;
        self.guard(local.len(), remote.len())?;

        let paths: BTreeSet<String> = local
            .keys()
            .chain(remote.keys())
            .chain(self.state.synced.keys())
            .cloned()
            .collect();
        for rel in paths {
            let Some(l) = self.local_side(&rel, local.get(&rel).copied()) else {
                continue;
            };
            let r = remote.get(&rel).cloned();
            let base = self.state.synced.get(&rel).cloned();
            let r_changed = match (&r, &base) {
                (None, None) => false,
                // Same content: take the new eTag in case only metadata moved.
                (Some(r), Some(b))
                    if r.etag == b.remote_etag || r.hash.as_deref() == Some(b.hash.as_str()) =>
                {
                    if let Some(s) = self.state.synced.get_mut(&rel) {
                        s.remote_etag = r.etag.clone();
                        s.remote_id = r.id.clone();
                    }
                    false
                }
                _ => true,
            };
            self.settle(&rel, &l, r.as_ref(), r_changed, &local, &remote)?;
        }
        Ok(())
    }

    fn settle(
        &mut self,
        rel: &str,
        l: &LocalSide,
        r: Option<&RemoteFile>,
        r_changed: bool,
        local: &BTreeMap<String, LocalFile>,
        remote: &BTreeMap<String, RemoteFile>,
    ) -> Result<(), SyncError> {
        let rule = self.pair.rules.for_path(rel).clone();
        match (l.changed, r_changed, &l.file, r) {
            (false, false, None, None) => {
                self.state.synced.remove(rel);
            }
            (false, false, _, _) => {}

            // Changed here only.
            (true, false, Some(_), r) => {
                let cond = match r {
                    Some(r) => Precondition::Match(r.etag.clone()),
                    None => Precondition::New,
                };
                self.upload(rel, l, cond, false)?;
            }
            (true, false, None, Some(r)) => self.delete_remote(rel, r)?,
            (true, false, None, None) => {
                self.state.synced.remove(rel);
            }

            // Changed on OneDrive only.
            (false, true, _, Some(r)) => self.download(rel, l, r)?,
            (false, true, Some(_), None) => self.delete_local(rel, l)?,
            (false, true, None, None) => {
                self.state.synced.remove(rel);
            }

            // Changed on both.
            (true, true, None, None) => {
                self.state.synced.remove(rel);
            }
            (true, true, Some(lf), Some(r)) => {
                if l.hash.is_some() && l.hash == r.hash {
                    self.adopt(rel, *lf, l.hash.clone().unwrap(), r);
                    return Ok(());
                }
                match rule.both_changed {
                    BothChanged::NewestWins => {
                        if lf.mtime_ms >= r.mtime_ms {
                            self.say(
                                Level::Info,
                                format!("{rel}: both changed; this copy is newer"),
                            );
                            self.upload(rel, l, Precondition::Match(r.etag.clone()), false)?;
                        } else {
                            self.say(
                                Level::Info,
                                format!("{rel}: both changed; OneDrive's is newer"),
                            );
                            self.download(rel, l, r)?;
                        }
                    }
                    BothChanged::KeepBoth => self.keep_both(rel, l, r, local, remote)?,
                }
            }
            // Edited on OneDrive, deleted here.
            (true, true, None, Some(r)) => match rule.delete_vs_edit {
                DeleteVsEdit::KeepEdit => {
                    self.say(
                        Level::Info,
                        format!("{rel}: deleted here but changed on OneDrive; keeping it"),
                    );
                    self.download(rel, l, r)?;
                }
                DeleteVsEdit::Delete => self.delete_remote(rel, r)?,
            },
            // Edited here, deleted on OneDrive.
            (true, true, Some(_), None) => match rule.delete_vs_edit {
                DeleteVsEdit::KeepEdit => {
                    self.say(
                        Level::Info,
                        format!("{rel}: deleted on OneDrive but changed here; keeping it"),
                    );
                    self.upload(rel, l, Precondition::New, false)?;
                }
                DeleteVsEdit::Delete => self.delete_local(rel, l)?,
            },
        }
        Ok(())
    }

    /// Both sides hold the same content: record it as synced.
    fn adopt(&mut self, rel: &str, lf: LocalFile, hash: String, r: &RemoteFile) {
        self.state.synced.insert(
            rel.to_string(),
            Synced {
                hash,
                local_size: lf.size,
                local_mtime_ms: lf.mtime_ms,
                remote_id: r.id.clone(),
                remote_etag: r.etag.clone(),
            },
        );
    }

    /// True if the local file is still as the scan found it.
    fn unchanged_since_scan(&self, rel: &str, l: &LocalSide) -> io::Result<bool> {
        Ok(local::stat(&self.full(rel))? == l.file)
    }

    fn upload(
        &mut self,
        rel: &str,
        l: &LocalSide,
        cond: Precondition,
        overwrite_on_conflict: bool,
    ) -> Result<(), SyncError> {
        let full = self.full(rel);
        let data = match std::fs::read(&full) {
            Ok(d) => d,
            Err(e) => {
                self.say(Level::Warn, format!("{rel}: can't read it: {e}"));
                self.report.failed += 1;
                return Ok(());
            }
        };
        let hash = quickxor::hash(&data);
        if Some(&hash) != l.hash.as_ref() || !self.unchanged_since_scan(rel, l)? {
            self.report.deferred += 1;
            return Ok(());
        }
        if self.pair.mode == Mode::TwoWay {
            if let Err(e) = self.ensure_dirs(rel) {
                return self.remote_failed(rel, "making its folder", e);
            }
        }
        let remote_path = self.pair.remote_path(rel);
        let mut result = self.drive.upload(&remote_path, &data, cond);
        if overwrite_on_conflict && matches!(result, Err(RemoteError::Conflict)) {
            self.say(
                Level::Warn,
                format!("{rel}: replacing OneDrive's copy (push mode)"),
            );
            result = self
                .drive
                .upload(&remote_path, &data, Precondition::Overwrite);
        }
        let item = match result {
            Ok(item) => item,
            Err(e) => return self.remote_failed(rel, "uploading", e),
        };
        self.record(rel, l.file.unwrap(), hash, &item);
        if self.pair.mode == Mode::TwoWay {
            self.state.tree.record_upload(rel, item);
        }
        self.report.uploaded += 1;
        self.say(Level::Info, format!("{rel}: uploaded"));
        self.save()?;
        Ok(())
    }

    /// Makes sure the folders above `rel` exist on OneDrive and are in the
    /// tree, so a later move or delete of them reaches the file.
    fn ensure_dirs(&mut self, rel: &str) -> Result<(), RemoteError> {
        let parts: Vec<&str> = rel.split('/').collect();
        for depth in 1..parts.len() {
            let dir = parts[..depth].join("/");
            if self.state.tree.folder_id(&dir).is_none() {
                let item = self.drive.ensure_folder(&self.pair.remote_path(&dir))?;
                self.state.tree.record_folder(item);
            }
        }
        Ok(())
    }

    fn record(&mut self, rel: &str, lf: LocalFile, hash: String, item: &RemoteItem) {
        self.state.synced.insert(
            rel.to_string(),
            Synced {
                hash,
                local_size: lf.size,
                local_mtime_ms: lf.mtime_ms,
                remote_id: item.id.clone(),
                remote_etag: item.etag.clone(),
            },
        );
    }

    fn download(&mut self, rel: &str, l: &LocalSide, r: &RemoteFile) -> Result<(), SyncError> {
        let data = match self.drive.download(&r.id) {
            Ok(d) => d,
            Err(e) => return self.remote_failed(rel, "downloading", e),
        };
        if !self.unchanged_since_scan(rel, l)? {
            self.report.deferred += 1;
            return Ok(());
        }
        let lf = match local::replace(&self.full(rel), &data) {
            Ok(f) => f,
            Err(e) => {
                self.say(Level::Warn, format!("{rel}: can't write it: {e}"));
                self.report.failed += 1;
                return Ok(());
            }
        };
        self.state.synced.insert(
            rel.to_string(),
            Synced {
                hash: quickxor::hash(&data),
                local_size: lf.size,
                local_mtime_ms: lf.mtime_ms,
                remote_id: r.id.clone(),
                remote_etag: r.etag.clone(),
            },
        );
        self.report.downloaded += 1;
        self.say(Level::Info, format!("{rel}: downloaded"));
        self.save()?;
        Ok(())
    }

    fn delete_remote(&mut self, rel: &str, r: &RemoteFile) -> Result<(), SyncError> {
        match self.drive.delete(&r.id, Some(&r.etag)) {
            Ok(()) | Err(RemoteError::NotFound) => {}
            Err(e) => return self.remote_failed(rel, "deleting", e),
        }
        self.state.tree.forget(&r.id);
        self.state.synced.remove(rel);
        self.report.deleted_remote += 1;
        self.say(
            Level::Info,
            format!("{rel}: moved to OneDrive's recycle bin"),
        );
        self.save()?;
        Ok(())
    }

    fn delete_local(&mut self, rel: &str, l: &LocalSide) -> Result<(), SyncError> {
        if !self.unchanged_since_scan(rel, l)? {
            self.report.deferred += 1;
            return Ok(());
        }
        if let Err(e) = local::delete(&self.full(rel), self.opts.to_trash) {
            self.say(Level::Warn, format!("{rel}: can't delete it: {e}"));
            self.report.failed += 1;
            return Ok(());
        }
        self.state.synced.remove(rel);
        self.report.deleted_local += 1;
        self.say(
            Level::Info,
            format!("{rel}: deleted here (deleted on OneDrive)"),
        );
        self.save()?;
        Ok(())
    }

    /// Keeps both copies: this computer's is renamed with a conflict name and
    /// uploaded; OneDrive's comes down under the original name.
    fn keep_both(
        &mut self,
        rel: &str,
        l: &LocalSide,
        r: &RemoteFile,
        local: &BTreeMap<String, LocalFile>,
        remote: &BTreeMap<String, RemoteFile>,
    ) -> Result<(), SyncError> {
        let free = |name: &String| {
            !local.contains_key(name)
                && !remote.contains_key(name)
                && !local::full_path(&self.pair.local, name).exists()
        };
        let conflict = (1..)
            .map(|n| conflict_name(rel, &self.opts.host, n))
            .find(free)
            .unwrap();
        if !self.unchanged_since_scan(rel, l)? {
            self.report.deferred += 1;
            return Ok(());
        }
        std::fs::rename(self.full(rel), self.full(&conflict))?;
        self.report.conflicts += 1;
        self.say(
            Level::Warn,
            format!("{rel}: changed on both sides; this computer's copy is now {conflict}"),
        );
        self.state.synced.remove(rel);
        self.save()?;

        let gone = LocalSide {
            file: None,
            hash: None,
            changed: false,
        };
        self.download(rel, &gone, r)?;

        let lf = local::stat(&self.full(&conflict))?;
        let side = LocalSide {
            file: lf,
            hash: l.hash.clone(),
            changed: true,
        };
        if lf.is_some() {
            self.upload(&conflict, &side, Precondition::New, false)?;
        }
        Ok(())
    }
}
