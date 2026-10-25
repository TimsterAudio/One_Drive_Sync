//! A worker thread that keeps a set of pairs in sync: it runs a pair when its
//! folder changes (watched with `notify`), when its poll interval comes round,
//! and on request, and reports back through events.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};

use crate::engine::{self, Level, PairConfig, Report, SyncError, SyncOptions};
use crate::folder::FolderDrive;
use crate::local::{self, TMP_SUFFIX};
use crate::remote::{RemoteDrive, RemoteError};

/// How long a folder must be quiet after a change before it is synced.
const DEBOUNCE: Duration = Duration::from_millis(1500);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PairStatus {
    Waiting,
    Syncing,
    /// Last run finished: what it did, and when (ms since the epoch).
    Done {
        report: Report,
        at_ms: i64,
    },
    /// Last run stopped; it is retried at the next poll.
    Failed(String),
    /// Signed out or the sign-in expired; retried at each poll.
    SignInNeeded,
}

#[derive(Clone, Debug)]
pub enum Event {
    Log {
        at_ms: i64,
        /// Index into the pairs last set, if the line is about one.
        pair: Option<usize>,
        level: Level,
        message: String,
    },
    Status {
        pair: usize,
        status: PairStatus,
    },
}

enum Msg {
    SetPairs(Vec<PairConfig>),
    SyncNow,
    Changed(usize, u64),
    Shutdown,
}

/// The handle the app keeps; dropping it stops the worker.
pub struct SyncService {
    tx: Sender<Msg>,
    events: Receiver<Event>,
    thread: Option<JoinHandle<()>>,
}

/// Where each pair keeps its state file inside `state_dir`.
pub fn state_path(state_dir: &Path, pair: &PairConfig) -> PathBuf {
    let mut h = Sha256::new();
    h.update(pair.local.to_string_lossy().as_bytes());
    h.update(b"\n");
    h.update(pair.remote_name().as_bytes());
    let digest = h.finalize();
    let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    state_dir.join(format!("pair-{id}.json"))
}

impl SyncService {
    /// Starts the worker, with `drive` for the pairs that sync with OneDrive.
    /// `wake` is called after each batch of events (an egui app passes
    /// `ctx.request_repaint`).
    pub fn start(
        drive: Box<dyn RemoteDrive + Send>,
        state_dir: PathBuf,
        options: SyncOptions,
        wake: Box<dyn Fn() + Send>,
    ) -> Self {
        Self::spawn(Some(drive), state_dir, options, wake)
    }

    /// Starts the worker for pairs whose other side is a folder
    /// ([`PairConfig::folders`]); OneDrive pairs fail with a message.
    pub fn start_folders(
        state_dir: PathBuf,
        options: SyncOptions,
        wake: Box<dyn Fn() + Send>,
    ) -> Self {
        Self::spawn(None, state_dir, options, wake)
    }

    fn spawn(
        drive: Option<Box<dyn RemoteDrive + Send>>,
        state_dir: PathBuf,
        options: SyncOptions,
        wake: Box<dyn Fn() + Send>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let (etx, events) = mpsc::channel();
        let watch_tx = tx.clone();
        let thread = std::thread::Builder::new()
            .name("folder-sync".into())
            .spawn(move || {
                Worker {
                    drive,
                    state_dir,
                    options,
                    events: etx,
                    wake,
                    watch_tx,
                    pairs: Vec::new(),
                    generation: 0,
                }
                .run(rx)
            })
            .expect("couldn't start the sync thread");
        Self {
            tx,
            events,
            thread: Some(thread),
        }
    }

    /// Replaces the pairs; each is synced straight away.
    pub fn set_pairs(&self, pairs: Vec<PairConfig>) {
        let _ = self.tx.send(Msg::SetPairs(pairs));
    }

    /// Syncs every pair now (after signing in, say).
    pub fn sync_now(&self) {
        let _ = self.tx.send(Msg::SyncNow);
    }

    /// Events since the last call.
    pub fn events(&self) -> Vec<Event> {
        self.events.try_iter().collect()
    }
}

impl Drop for SyncService {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct PairRun {
    config: PairConfig,
    state_path: PathBuf,
    next_poll: Instant,
    /// A change was seen; sync at this time unless more changes come.
    pending: Option<Instant>,
    last_error: Option<String>,
    /// The other side, for a pair synced with a folder.
    folder: Option<FolderDrive>,
    _watcher: Option<RecommendedWatcher>,
}

struct Worker {
    /// OneDrive, for pairs without a share folder.
    drive: Option<Box<dyn RemoteDrive + Send>>,
    state_dir: PathBuf,
    options: SyncOptions,
    events: Sender<Event>,
    wake: Box<dyn Fn() + Send>,
    watch_tx: Sender<Msg>,
    pairs: Vec<PairRun>,
    /// Bumped when the pairs are replaced, so old watchers' messages are dropped.
    generation: u64,
}

impl Worker {
    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn log(&self, pair: Option<usize>, level: Level, message: String) {
        self.emit(Event::Log {
            at_ms: local::now_ms(),
            pair,
            level,
            message,
        });
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            let now = Instant::now();
            let next = self
                .pairs
                .iter()
                .map(|p| p.pending.map_or(p.next_poll, |t| t.min(p.next_poll)))
                .min();
            let msg = match next {
                Some(t) => rx.recv_timeout(t.saturating_duration_since(now)),
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match msg {
                Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
                Ok(Msg::SetPairs(pairs)) => self.set_pairs(pairs),
                Ok(Msg::SyncNow) => {
                    for p in &mut self.pairs {
                        p.pending = Some(Instant::now());
                    }
                }
                Ok(Msg::Changed(i, generation)) => {
                    if generation == self.generation {
                        if let Some(p) = self.pairs.get_mut(i) {
                            p.pending = Some(Instant::now() + DEBOUNCE);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            let now = Instant::now();
            for i in 0..self.pairs.len() {
                let p = &self.pairs[i];
                if p.pending.is_some_and(|t| t <= now) || p.next_poll <= now {
                    self.sync_pair(i);
                }
            }
            (self.wake)();
        }
    }

    fn set_pairs(&mut self, pairs: Vec<PairConfig>) {
        self.generation += 1;
        let now = Instant::now();
        self.pairs = pairs
            .into_iter()
            .enumerate()
            .map(|(i, config)| {
                let watcher = self.watch(i, &config.local);
                let folder = config.share.clone().map(FolderDrive::new);
                PairRun {
                    state_path: state_path(&self.state_dir, &config),
                    config,
                    next_poll: now,
                    pending: None,
                    last_error: None,
                    folder,
                    _watcher: watcher,
                }
            })
            .collect();
        for i in 0..self.pairs.len() {
            self.emit(Event::Status {
                pair: i,
                status: PairStatus::Waiting,
            });
        }
    }

    fn watch(&self, index: usize, folder: &Path) -> Option<RecommendedWatcher> {
        let tx = self.watch_tx.clone();
        let generation = self.generation;
        let handler = move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if event.kind.is_access() {
                return;
            }
            let ours = event
                .paths
                .iter()
                .all(|p| p.to_string_lossy().ends_with(TMP_SUFFIX));
            if !ours {
                let _ = tx.send(Msg::Changed(index, generation));
            }
        };
        match notify::recommended_watcher(handler) {
            Ok(mut w) => match w.watch(folder, RecursiveMode::Recursive) {
                Ok(()) => Some(w),
                Err(e) => {
                    self.log(
                        Some(index),
                        Level::Warn,
                        format!("can't watch {} ({e}); polling only", folder.display()),
                    );
                    None
                }
            },
            Err(e) => {
                self.log(
                    Some(index),
                    Level::Warn,
                    format!("no file watcher ({e}); polling only"),
                );
                None
            }
        }
    }

    fn sync_pair(&mut self, i: usize) {
        self.emit(Event::Status {
            pair: i,
            status: PairStatus::Syncing,
        });
        (self.wake)();
        let mut lines = Vec::new();
        let p = &mut self.pairs[i];
        let drive: Option<&mut dyn RemoteDrive> = match &mut p.folder {
            Some(f) => Some(f),
            None => self.drive.as_deref_mut().map(|d| d as &mut dyn RemoteDrive),
        };
        let result = match drive {
            Some(drive) => engine::sync(
                &p.config,
                &self.options,
                &p.state_path,
                drive,
                &mut |line| lines.push(line),
            ),
            None => Err(SyncError::Remote(RemoteError::Other(
                "this pair needs OneDrive, which isn't set up here".into(),
            ))),
        };
        for line in lines {
            self.log(Some(i), line.level, line.message);
        }

        let poll = Duration::from_secs(self.pairs[i].config.poll_secs.max(5));
        let mut next_poll = Instant::now() + poll;
        let mut pending = None;
        let status = match result {
            Ok(report) => {
                if report.deferred > 0 {
                    pending = Some(Instant::now() + DEBOUNCE * 2);
                }
                if report.changed() || report.failed > 0 {
                    self.log(Some(i), Level::Info, format!("synced: {report}"));
                }
                self.pairs[i].last_error = None;
                PairStatus::Done {
                    report,
                    at_ms: local::now_ms(),
                }
            }
            Err(SyncError::Remote(RemoteError::Auth(m))) => {
                self.note_error(i, format!("sign-in needed: {m}"));
                PairStatus::SignInNeeded
            }
            Err(e) => {
                if let SyncError::Remote(RemoteError::Throttled(d)) = &e {
                    next_poll = Instant::now() + (*d).max(poll);
                }
                let msg = e.to_string();
                self.note_error(i, msg.clone());
                PairStatus::Failed(msg)
            }
        };
        let p = &mut self.pairs[i];
        p.next_poll = next_poll;
        p.pending = pending;
        self.emit(Event::Status { pair: i, status });
    }

    /// Logs an error once, not again on every retry.
    fn note_error(&mut self, i: usize, msg: String) {
        if self.pairs[i].last_error.as_ref() != Some(&msg) {
            self.log(Some(i), Level::Error, msg.clone());
            self.pairs[i].last_error = Some(msg);
        }
    }
}
