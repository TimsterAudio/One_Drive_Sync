//! The engine against the in-memory drive: two computers ("atlas" and
//! "bramble") sharing one OneDrive, no network.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use one_drive_sync::{
    sync, FakeDrive, Mode, PairConfig, RemoteError, Report, SyncError, SyncOptions,
};
use tempfile::TempDir;

struct Machine {
    dir: TempDir,
    state: TempDir,
    pair: PairConfig,
    opts: SyncOptions,
    /// Each write gets a later time, so edits are never mistaken for no change.
    clock_ms: std::cell::Cell<i64>,
}

const REMOTE: &str = "Waypoint/trips";

impl Machine {
    fn new(host: &str, mode: Mode) -> Self {
        let dir = TempDir::new().unwrap();
        let pair = PairConfig::new(dir.path(), REMOTE, mode);
        Self {
            dir,
            state: TempDir::new().unwrap(),
            pair,
            opts: SyncOptions {
                host: host.into(),
                to_trash: false,
                settle_ms: 0,
                guard_min: 5,
            },
            clock_ms: std::cell::Cell::new(1_700_000_000_000),
        }
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn sync(&self, drive: &FakeDrive) -> Report {
        self.try_sync(drive).expect("sync failed")
    }

    fn try_sync(&self, drive: &FakeDrive) -> Result<Report, SyncError> {
        let mut d = drive.clone();
        let state = self.state.path().join("state.json");
        sync(&self.pair, &self.opts, &state, &mut d, &mut |l| {
            eprintln!("[{}] {:?} {}", self.opts.host, l.level, l.message)
        })
    }

    /// Writes a file, stamped `at_ms` or else a moment after the last write.
    fn write_at(&self, rel: &str, data: &[u8], at_ms: Option<i64>) {
        let p = self.path(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, data).unwrap();
        let t = at_ms.unwrap_or_else(|| {
            self.clock_ms.set(self.clock_ms.get() + 1000);
            self.clock_ms.get()
        });
        let f = fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_millis(t as u64))
            .unwrap();
    }

    fn write(&self, rel: &str, data: &[u8]) {
        self.write_at(rel, data, None)
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        fs::read(self.path(rel)).ok()
    }

    fn remove(&self, rel: &str) {
        fs::remove_file(self.path(rel)).unwrap();
    }

    fn files(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![self.dir.path().to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(d).unwrap() {
                let e = e.unwrap();
                if e.file_type().unwrap().is_dir() {
                    stack.push(e.path());
                } else {
                    let rel = e.path().strip_prefix(self.dir.path()).unwrap().to_owned();
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            }
        }
        out.sort();
        out
    }
}

fn remote(path: &str) -> String {
    format!("{REMOTE}/{path}")
}

fn remote_files(drive: &FakeDrive) -> Vec<String> {
    let prefix = format!("{REMOTE}/");
    drive
        .files()
        .into_keys()
        .filter_map(|k| k.strip_prefix(&prefix).map(str::to_string))
        .collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn first_sync_merges_both_sides() {
    let drive = FakeDrive::new();
    drive.put(&remote("lakes.trip.json"), b"remote trip", None);
    let a = Machine::new("atlas", Mode::TwoWay);
    a.write("coast.trip.json", b"local trip");
    a.write("pages/coast.html", b"<p>coast</p>");

    let r = a.sync(&drive);
    assert_eq!((r.uploaded, r.downloaded), (2, 1));
    assert_eq!(a.read("lakes.trip.json").unwrap(), b"remote trip");
    assert_eq!(
        drive.read(&remote("pages/coast.html")).unwrap(),
        b"<p>coast</p>"
    );

    // Nothing more to do.
    let r = a.sync(&drive);
    assert!(!r.changed(), "{r:?}");
}

#[test]
fn identical_copies_on_both_sides_are_adopted() {
    let drive = FakeDrive::new();
    drive.put(&remote("lakes.trip.json"), b"same", None);
    let a = Machine::new("atlas", Mode::TwoWay);
    a.write("lakes.trip.json", b"same");
    let r = a.sync(&drive);
    assert_eq!(r, Report::default());
    assert_eq!(a.files(), ["lakes.trip.json"]);
}

#[test]
fn trips_changed_on_both_sides_keep_both() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    let b = Machine::new("bramble", Mode::TwoWay);
    a.write("lakes.trip.json", b"v1");
    a.sync(&drive);
    b.sync(&drive);

    a.write("lakes.trip.json", b"atlas edit");
    b.write("lakes.trip.json", b"bramble edit");
    a.sync(&drive); // atlas gets there first
    let r = b.sync(&drive);
    assert_eq!(r.conflicts, 1);

    let conflict = "lakes (conflict from bramble).trip.json";
    assert_eq!(b.read("lakes.trip.json").unwrap(), b"atlas edit");
    assert_eq!(b.read(conflict).unwrap(), b"bramble edit");
    a.sync(&drive);
    assert_eq!(a.read(conflict).unwrap(), b"bramble edit");
    assert_eq!(a.files(), b.files());
    assert_eq!(remote_files(&drive), a.files());

    // Settled: nothing more moves.
    assert!(!a.sync(&drive).changed());
    assert!(!b.sync(&drive).changed());
}

#[test]
fn locks_and_pages_go_to_the_newest() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    a.write("lakes.trip.lock", b"atlas holds");
    a.write("lakes.html", b"old page");
    a.sync(&drive);
    let t = now_ms();

    // Here newer for the lock, OneDrive newer for the page.
    a.write_at("lakes.trip.lock", b"atlas again", Some(t + 10_000));
    drive.put(&remote("lakes.trip.lock"), b"bramble holds", Some(t));
    a.write_at("lakes.html", b"atlas page", Some(t));
    drive.put(&remote("lakes.html"), b"bramble page", Some(t + 10_000));

    let r = a.sync(&drive);
    assert_eq!(r.conflicts, 0);
    assert_eq!(
        drive.read(&remote("lakes.trip.lock")).unwrap(),
        b"atlas again"
    );
    assert_eq!(a.read("lakes.html").unwrap(), b"bramble page");
    assert_eq!(a.files(), ["lakes.html", "lakes.trip.lock"]);
}

#[test]
fn deletions_sync_both_ways_to_the_recycle_bin() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    a.write("one.trip.json", b"1");
    a.write("two.trip.json", b"2");
    a.sync(&drive);

    a.remove("one.trip.json");
    drive.remove(&remote("two.trip.json"));
    let r = a.sync(&drive);
    assert_eq!((r.deleted_remote, r.deleted_local), (1, 1));
    assert!(a.files().is_empty());
    assert!(remote_files(&drive).is_empty());
    let mut binned = drive.recycled();
    binned.sort();
    assert_eq!(binned, [remote("one.trip.json"), remote("two.trip.json")]);
}

#[test]
fn delete_versus_edit() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    for f in ["x.trip.json", "y.trip.json", "x.trip.lock", "y.trip.lock"] {
        a.write(f, b"v1");
    }
    a.sync(&drive);

    // Trips: the edit survives whichever side deleted.
    a.remove("x.trip.json");
    drive.put(&remote("x.trip.json"), b"edited there", None);
    a.write("y.trip.json", b"edited here");
    drive.remove(&remote("y.trip.json"));
    // Locks: the deletion wins whichever side deleted.
    a.remove("x.trip.lock");
    drive.put(&remote("x.trip.lock"), b"edited there", None);
    a.write("y.trip.lock", b"edited here");
    drive.remove(&remote("y.trip.lock"));

    a.sync(&drive);
    assert_eq!(a.read("x.trip.json").unwrap(), b"edited there");
    assert_eq!(drive.read(&remote("y.trip.json")).unwrap(), b"edited here");
    assert_eq!(a.files(), ["x.trip.json", "y.trip.json"]);
    assert_eq!(remote_files(&drive), a.files());
}

#[test]
fn lock_churn_between_two_computers() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    let b = Machine::new("bramble", Mode::TwoWay);
    a.write("lakes.trip.json", b"trip");
    a.sync(&drive);
    b.sync(&drive);

    for round in 0..5 {
        let (taker, other) = if round % 2 == 0 { (&a, &b) } else { (&b, &a) };
        let body = format!("held by {} round {round}", taker.opts.host);
        taker.write("lakes.trip.lock", body.as_bytes());
        taker.sync(&drive);
        other.sync(&drive);
        assert_eq!(other.read("lakes.trip.lock").unwrap(), body.as_bytes());

        // Released.
        taker.remove("lakes.trip.lock");
        taker.sync(&drive);
        other.sync(&drive);
        assert!(other.read("lakes.trip.lock").is_none(), "round {round}");
    }

    // Both take the lock at once: the newer one stands everywhere.
    let t = now_ms();
    a.write_at("lakes.trip.lock", b"atlas", Some(t));
    b.write_at("lakes.trip.lock", b"bramble", Some(t + 5000));
    a.sync(&drive);
    b.sync(&drive);
    a.sync(&drive);
    assert_eq!(a.read("lakes.trip.lock").unwrap(), b"bramble");
    assert_eq!(b.read("lakes.trip.lock").unwrap(), b"bramble");
    assert_eq!(a.files(), b.files());
}

#[test]
fn offline_then_reconnect_loses_nothing() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    a.write("lakes.trip.json", b"v1");
    a.write("coast.trip.json", b"v1");
    a.write("old.trip.json", b"v1");
    a.sync(&drive);

    drive.set_offline(true);
    a.write("lakes.trip.json", b"edited offline");
    a.write("new.trip.json", b"made offline");
    a.remove("old.trip.json");
    let err = a.try_sync(&drive).unwrap_err();
    assert!(matches!(err, SyncError::Remote(RemoteError::Offline(_))));
    // Meanwhile another computer edits a different trip.
    drive.set_offline(false);
    drive.put(&remote("coast.trip.json"), b"edited elsewhere", None);

    a.sync(&drive);
    assert_eq!(
        drive.read(&remote("lakes.trip.json")).unwrap(),
        b"edited offline"
    );
    assert_eq!(
        drive.read(&remote("new.trip.json")).unwrap(),
        b"made offline"
    );
    assert!(drive.read(&remote("old.trip.json")).is_none());
    assert_eq!(a.read("coast.trip.json").unwrap(), b"edited elsewhere");
    assert_eq!(remote_files(&drive), a.files());
}

#[test]
fn goes_offline_halfway_through() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    for i in 0..4 {
        a.write(&format!("t{i}.trip.json"), b"v1");
    }
    // Going offline during the run: the delta succeeds, uploads fail.
    let mut d = drive.clone();
    let state = a.state.path().join("state.json");
    let mut calls = 0;
    let _ = sync(
        &a.pair,
        &a.opts,
        &state,
        &mut OfflineAfter(&mut d, 3, &mut calls),
        &mut |_| {},
    );
    let r = a.sync(&drive);
    assert_eq!(r.conflicts, 0);
    assert_eq!(remote_files(&drive), a.files());
    assert!(!a.sync(&drive).changed());
}

/// Wraps a drive so it goes offline after `n` calls.
struct OfflineAfter<'a>(&'a mut FakeDrive, usize, &'a mut usize);

impl one_drive_sync::RemoteDrive for OfflineAfter<'_> {
    fn ensure_folder(
        &mut self,
        p: &str,
    ) -> Result<one_drive_sync::remote::RemoteItem, RemoteError> {
        self.tick()?;
        self.0.ensure_folder(p)
    }
    fn delta(
        &mut self,
        id: &str,
        c: Option<&str>,
    ) -> Result<one_drive_sync::remote::Delta, RemoteError> {
        self.tick()?;
        self.0.delta(id, c)
    }
    fn download(&mut self, id: &str) -> Result<Vec<u8>, RemoteError> {
        self.tick()?;
        self.0.download(id)
    }
    fn upload(
        &mut self,
        p: &str,
        d: &[u8],
        c: one_drive_sync::Precondition,
    ) -> Result<one_drive_sync::remote::RemoteItem, RemoteError> {
        self.tick()?;
        self.0.upload(p, d, c)
    }
    fn delete(&mut self, id: &str, e: Option<&str>) -> Result<(), RemoteError> {
        self.tick()?;
        self.0.delete(id, e)
    }
}

impl OfflineAfter<'_> {
    fn tick(&mut self) -> Result<(), RemoteError> {
        *self.2 += 1;
        if *self.2 > self.1 {
            Err(RemoteError::Offline("unplugged".into()))
        } else {
            Ok(())
        }
    }
}

#[test]
fn remote_folder_delete_and_rename_reach_local() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    a.write("2025/alps.trip.json", b"alps");
    a.write("2025/pages/alps.html", b"page");
    a.write("2026/lakes.trip.json", b"lakes");
    a.sync(&drive);

    drive.rename(&remote("2026"), &remote("current"));
    drive.remove(&remote("2025"));
    a.sync(&drive);
    assert_eq!(a.files(), ["current/lakes.trip.json"]);
    assert_eq!(a.read("current/lakes.trip.json").unwrap(), b"lakes");
}

#[test]
fn forced_resync_moves_nothing() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    a.write("lakes.trip.json", b"v1");
    a.sync(&drive);
    drive.force_resync();
    drive.put(&remote("coast.trip.json"), b"new", None);
    let r = a.sync(&drive);
    assert_eq!((r.uploaded, r.downloaded, r.conflicts), (0, 1, 0));
}

#[test]
fn push_mode_only_sends() {
    let drive = FakeDrive::new();
    drive.put(&remote("someone-elses.txt"), b"leave me", None);
    let a = Machine::new("atlas", Mode::Push);
    a.write("today.json", b"rain");
    a.write("tomorrow.json", b"sun");
    a.sync(&drive);
    assert_eq!(a.files(), ["today.json", "tomorrow.json"]);
    assert_eq!(
        remote_files(&drive),
        ["someone-elses.txt", "today.json", "tomorrow.json"]
    );

    // OneDrive's copy changed; push replaces it. A deletion goes up too.
    drive.put(&remote("today.json"), b"edited there", None);
    a.write("today.json", b"storms");
    a.remove("tomorrow.json");
    a.sync(&drive);
    assert_eq!(drive.read(&remote("today.json")).unwrap(), b"storms");
    assert_eq!(remote_files(&drive), ["someone-elses.txt", "today.json"]);
    // Nothing came down.
    assert_eq!(a.files(), ["today.json"]);
}

#[test]
fn an_emptied_folder_stops_the_run() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    for i in 0..6 {
        a.write(&format!("t{i}.trip.json"), b"v1");
    }
    a.sync(&drive);
    for i in 0..6 {
        a.remove(&format!("t{i}.trip.json"));
    }
    assert!(matches!(a.try_sync(&drive), Err(SyncError::Guard(_))));
    assert_eq!(remote_files(&drive).len(), 6);
}

#[test]
fn contents_are_copied_byte_for_byte() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    let b = Machine::new("bramble", Mode::TwoWay);
    let odd: Vec<u8> = (0..=255u8).cycle().take(70_000).collect();
    a.write("blob.bin", &odd);
    a.write("bom.trip.json", b"\xEF\xBB\xBF{ \"a\": 1 }\r\n");
    a.sync(&drive);
    b.sync(&drive);
    assert_eq!(b.read("blob.bin").unwrap(), odd);
    assert_eq!(
        b.read("bom.trip.json").unwrap(),
        b"\xEF\xBB\xBF{ \"a\": 1 }\r\n"
    );
}

#[test]
fn ignored_and_temporary_files_stay_put() {
    let drive = FakeDrive::new();
    let a = Machine::new("atlas", Mode::TwoWay);
    a.write(".DS_Store", b"x");
    a.write("lakes.trip.json.odsync-tmp", b"x");
    a.write("lakes.trip.json", b"trip");
    a.sync(&drive);
    assert_eq!(remote_files(&drive), ["lakes.trip.json"]);
}
