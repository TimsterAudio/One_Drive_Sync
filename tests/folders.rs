//! Folder pairs: a local folder synced with another folder standing in for a
//! mounted Windows share.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH};

use one_drive_sync::folder::TRASH_DIR;
use one_drive_sync::{
    sync, Event, FolderDrive, Mode, PairConfig, PairStatus, RemoteError, Report, SyncError,
    SyncOptions, SyncService,
};
use tempfile::TempDir;

struct Setup {
    local: TempDir,
    share: TempDir,
    state: TempDir,
    pair: PairConfig,
    opts: SyncOptions,
    drive: FolderDrive,
}

fn setup(mode: Mode) -> Setup {
    let local = TempDir::new().unwrap();
    let share = TempDir::new().unwrap();
    let pair = PairConfig::folders(local.path(), share.path(), mode);
    Setup {
        drive: FolderDrive::new(share.path()),
        local,
        share,
        state: TempDir::new().unwrap(),
        pair,
        opts: SyncOptions {
            host: "linuxbox".into(),
            to_trash: false,
            settle_ms: 0,
            guard_min: 1,
        },
    }
}

fn write(root: &Path, rel: &str, data: &[u8], at_ms: u64) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(&p, data).unwrap();
    fs::File::options()
        .write(true)
        .open(&p)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_millis(at_ms))
        .unwrap();
}

fn read(root: &Path, rel: &str) -> Option<Vec<u8>> {
    fs::read(root.join(rel)).ok()
}

fn files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(d).unwrap() {
            let e = e.unwrap();
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel == TRASH_DIR {
                continue;
            }
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push(rel);
            }
        }
    }
    out.sort();
    out
}

impl Setup {
    fn try_sync(&mut self) -> Result<Report, SyncError> {
        let state = self.state.path().join("state.json");
        sync(&self.pair, &self.opts, &state, &mut self.drive, &mut |l| {
            eprintln!("{:?} {}", l.level, l.message)
        })
    }

    fn sync(&mut self) -> Report {
        self.try_sync().expect("sync failed")
    }

    fn l(&self) -> &Path {
        self.local.path()
    }

    fn s(&self) -> &Path {
        self.share.path()
    }
}

const T: u64 = 1_750_000_000_000;

#[test]
fn two_way_merges_edits_and_deletes() {
    let mut x = setup(Mode::TwoWay);
    write(x.l(), "lakes.trip.json", b"from linux", T);
    write(x.s(), "coast.trip.json", b"from windows", T);
    write(x.s(), "pages/coast.html", b"<p>", T);
    let r = x.sync();
    assert_eq!((r.uploaded, r.downloaded), (1, 2));
    assert_eq!(files(x.l()), files(x.s()));
    assert!(!x.sync().changed());

    // An edit on each side, and a deletion on the share.
    write(x.l(), "lakes.trip.json", b"linux v2", T + 1000);
    write(x.s(), "coast.trip.json", b"windows v2", T + 1000);
    fs::remove_file(x.s().join("pages/coast.html")).unwrap();
    x.sync();
    assert_eq!(read(x.s(), "lakes.trip.json").unwrap(), b"linux v2");
    assert_eq!(read(x.l(), "coast.trip.json").unwrap(), b"windows v2");
    assert!(read(x.l(), "pages/coast.html").is_none());

    // A local deletion goes to the share's trash, not for good.
    fs::remove_file(x.l().join("lakes.trip.json")).unwrap();
    let r = x.sync();
    assert_eq!(r.deleted_remote, 1);
    assert!(read(x.s(), "lakes.trip.json").is_none());
    assert!(x.s().join(TRASH_DIR).is_dir());
    assert_eq!(files(x.l()), files(x.s()));
}

#[test]
fn identical_files_are_adopted_without_copying() {
    let mut x = setup(Mode::TwoWay);
    write(x.l(), "a.trip.json", b"same", T);
    write(x.s(), "a.trip.json", b"same", T + 5000);
    assert_eq!(x.sync(), Report::default());
}

#[test]
fn both_changed_trips_keep_both_and_locks_go_newest() {
    let mut x = setup(Mode::TwoWay);
    write(x.l(), "a.trip.json", b"v1", T);
    write(x.l(), "a.trip.lock", b"v1", T);
    x.sync();

    write(x.l(), "a.trip.json", b"linux edit", T + 1000);
    write(x.s(), "a.trip.json", b"windows edit", T + 2000);
    write(x.l(), "a.trip.lock", b"linux lock", T + 9000);
    write(x.s(), "a.trip.lock", b"windows lock", T + 3000);
    let r = x.sync();
    assert_eq!(r.conflicts, 1);

    let conflict = "a (conflict from linuxbox).trip.json";
    assert_eq!(read(x.l(), "a.trip.json").unwrap(), b"windows edit");
    assert_eq!(read(x.l(), conflict).unwrap(), b"linux edit");
    assert_eq!(read(x.s(), conflict).unwrap(), b"linux edit");
    assert_eq!(read(x.s(), "a.trip.lock").unwrap(), b"linux lock");
    assert_eq!(files(x.l()), files(x.s()));
    assert!(!x.sync().changed());
}

#[test]
fn an_unmounted_share_deletes_nothing() {
    let mut y = setup(Mode::TwoWay);
    write(y.l(), "a.trip.json", b"a", T);
    y.sync();
    // The share's folder vanishes (unmounted, PC off).
    let share = y.share.path().to_path_buf();
    fs::rename(&share, share.with_extension("away")).unwrap();
    let err = y.try_sync().unwrap_err();
    assert!(
        matches!(err, SyncError::Remote(RemoteError::Offline(_))),
        "{err}"
    );
    assert!(read(y.l(), "a.trip.json").is_some());
    fs::rename(share.with_extension("away"), &share).unwrap();

    // There but empty, like a mount point with nothing mounted: stopped,
    // nothing deleted.
    fs::remove_file(share.join("a.trip.json")).unwrap();
    assert!(matches!(y.try_sync(), Err(SyncError::Guard(_))));
    assert!(read(y.l(), "a.trip.json").is_some());
}

#[test]
fn push_copies_to_the_share_only() {
    let mut x = setup(Mode::Push);
    write(x.l(), "today.json", b"rain", T);
    write(x.s(), "theirs.txt", b"keep", T);
    x.sync();
    assert_eq!(read(x.s(), "today.json").unwrap(), b"rain");
    assert!(read(x.l(), "theirs.txt").is_none());

    write(x.s(), "today.json", b"edited on windows", T + 500);
    write(x.l(), "today.json", b"storms", T + 1000);
    x.sync();
    assert_eq!(read(x.s(), "today.json").unwrap(), b"storms");
}

#[test]
fn the_worker_syncs_folder_pairs() {
    let local = TempDir::new().unwrap();
    let share = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    write(share.path(), "w.trip.json", b"w", T);
    let opts = SyncOptions {
        to_trash: false,
        settle_ms: 0,
        ..Default::default()
    };
    let service = SyncService::start_folders(state.path().to_path_buf(), opts, Box::new(|| {}));
    let mut pair = PairConfig::folders(local.path(), share.path(), Mode::TwoWay);
    pair.poll_secs = 3600;
    service.set_pairs(vec![pair]);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut done = false;
    while !done && Instant::now() < deadline {
        done = service.events().iter().any(|e| {
            matches!(
                e,
                Event::Status {
                    status: PairStatus::Done { .. },
                    ..
                }
            )
        });
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(done);
    assert_eq!(read(local.path(), "w.trip.json").unwrap(), b"w");
}
