//! The worker thread: syncs on start, on a local change, and on request.

use std::time::{Duration, Instant};

use one_drive_sync::{Event, FakeDrive, Mode, PairConfig, PairStatus, SyncOptions, SyncService};
use tempfile::TempDir;

fn wait_for(service: &SyncService, mut done: impl FnMut(&Event) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        for e in service.events() {
            if done(&e) {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out");
}

fn synced(e: &Event) -> bool {
    matches!(
        e,
        Event::Status {
            status: PairStatus::Done { .. },
            ..
        }
    )
}

#[test]
fn syncs_on_start_on_change_and_on_request() {
    let drive = FakeDrive::new();
    let dir = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    std::fs::write(dir.path().join("a.trip.json"), b"a").unwrap();

    let opts = SyncOptions {
        to_trash: false,
        settle_ms: 0,
        ..Default::default()
    };
    let service = SyncService::start(
        Box::new(drive.clone()),
        state.path().to_path_buf(),
        opts,
        Box::new(|| {}),
    );
    let mut pair = PairConfig::new(dir.path(), "trips", Mode::TwoWay);
    pair.poll_secs = 3600;
    service.set_pairs(vec![pair]);
    wait_for(&service, synced);
    assert_eq!(drive.read("trips/a.trip.json").unwrap(), b"a");

    // A local change is picked up by the watcher.
    std::fs::write(dir.path().join("b.trip.json"), b"b").unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while drive.read("trips/b.trip.json").is_none() && Instant::now() < deadline {
        service.events();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        drive.read("trips/b.trip.json").is_some(),
        "watcher didn't fire"
    );

    drive.put("trips/c.trip.json", b"c", None);
    service.sync_now();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !dir.path().join("c.trip.json").exists() && Instant::now() < deadline {
        service.events();
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(dir.path().join("c.trip.json").exists());
}
