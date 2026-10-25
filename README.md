# One_Drive_Sync

A small Rust crate that keeps local folders in sync with a personal OneDrive over Microsoft Graph. A standalone sync agent uses it first; later Waypoint, Isobar and the iOS app will too. It has no ties to any of them: each pulls it in as a git dependency pinned to a commit.

```toml
[dependencies]
one_drive_sync = { git = "https://github.com/TimsterAudio/One_Drive_Sync", rev = "<commit>" }
```

**Status: trial (0.1.0).** It works end to end against the in-memory drive, and the agent builds for Linux and Windows. It still needs testing against a real OneDrive (see [Trying it](#trying-it)).

## What's in it

| Module | What it does |
|---|---|
| `auth` | Sign-in with a personal Microsoft account: auth code + PKCE, through the system browser and an `http://localhost` loopback redirect. A public client, so no secret. The refresh token goes in the OS keychain (`keyring`: Credential Manager, Keychain, Secret Service). |
| `graph` | `GraphDrive`: list (delta), download, upload, delete over Graph. |
| `fake` | `FakeDrive`: an in-memory drive with OneDrive's behaviour where it matters (eTags, conditional uploads, recycle bin, delta that reports a deleted folder without its children). |
| `engine` | One sync run of a pair. |
| `rules` | Per-pattern rules for files changed on both sides. |
| `state` | The per-pair state file. |
| `worker` | `SyncService`: a thread that runs the pairs on file changes (`notify`), on a poll interval, and on request, and reports events. |
| `agent/` | The trial egui window (`odsync-agent`). |

All I/O is blocking and runs on the worker thread, which suits egui apps: the UI only reads events. **File contents are never parsed or changed**; files are copied byte for byte and compared by hash.

## Remote operations

- **Listing** uses delta queries on the pair's folder, and the delta link is saved between runs. If OneDrive answers `410 Gone`, the next query lists the folder from scratch.
- **Download:** `GET /items/{id}/content`. The file is written beside its target and then renamed into place.
- **Upload:** `PUT .../root:/path:/content`. A new file uses `conflictBehavior=fail`; a changed file sends `If-Match: <eTag>`, so the upload fails rather than overwrite a change someone else made. Simple uploads stop at 4 MB, and larger files are skipped with a log line (upload sessions are left for after the trial).
- **Delete:** `DELETE /items/{id}` with `If-Match`. On a personal OneDrive this puts the item in the recycle bin.
- **Errors:** being offline, signed out or throttled (429/503, honouring `Retry-After`) stops the run, and the next poll retries it. Any other error affects only its own file.

## Access: app folder or whole drive

`DriveScope::AppFolder` asks for `Files.ReadWrite.AppFolder` and puts every pair under `Apps/<app name>/` on OneDrive. `DriveScope::FullDrive` asks for `Files.ReadWrite` and starts pairs at the top of the drive. Both scopes add `offline_access`.

**The recommendation is the app folder.** It is least-privilege, and it keeps everything in one tidy place. It works on one condition: everything that shares files must sign in with the **same app registration** (the same client id), because the app folder is named after the registration. One registration can serve the agent, Waypoint, Isobar and the iOS app (add an iOS redirect URI alongside `http://localhost`). If something outside that set needs to see the files (OneDrive's own website aside), use the whole drive instead.

## Sync pairs

A pair is a local folder, a OneDrive folder, a mode and a poll interval:

- **Push:** local changes go up, deletions included. A copy that changed on OneDrive is replaced (with a warning in the log). Nothing comes down, and OneDrive files that this pair never uploaded are left alone. This mode is for the weather files.
- **TwoWay:** changes go both ways, and the rules settle files that changed on both sides.

The **state file** for each pair records, for every path, the last-synced content hash (OneDrive's QuickXorHash, which can be computed locally), the local size and modification time, and the remote id and eTag. It also holds the remote tree that the delta queries have described, and the delta link. A local file is re-hashed only when its size or time has moved, and a touched but unchanged file counts as unchanged. A remote file counts as changed when its eTag moved and its hash differs.

On the first sync, a file that already exists on both sides with the same content is simply adopted. There is no conflict and no transfer.

### Rules

Rules are tried in order on the file name (`*` and `?` wildcards, case-insensitive), and the first match decides:

| Pattern | Both sides changed | One side deleted, the other edited |
|---|---|---|
| `*.trip.json` | **Keep both.** OneDrive's copy keeps the name; this computer's copy becomes `name (conflict from HOST).trip.json` and syncs too | The edit survives |
| `*.trip.lock` | **Newest wins** (by modification time) | **The deletion wins** (a released lock stays released) |
| `*.html` | **Newest wins** | The edit survives |
| anything else | Keep both | The edit survives |

Deletions sync both ways when the other side hasn't changed. Remote deletions go to OneDrive's recycle bin and local deletions go to the system wastebasket. Some files never sync: `*.odsync-tmp` (the crate's own temporary files), `.DS_Store`, `Thumbs.db`, `desktop.ini`, `~$*` and `.~lock.*`.

Two safety checks protect the data:

- A file modified in the last 2 seconds waits for the next run, in case it is still being written. Any file that changes between the scan and the copy also waits.
- If one side is empty but at least 5 files were synced, the run stops rather than delete everything on the other side. That folder is more likely missing than emptied.

### Change detection

A watcher (`notify`) on each local folder triggers a sync once the folder has been quiet for 1.5 s. Each pair also polls on its own interval (20 s suits Waypoint), which asks OneDrive for changes and catches anything the watcher missed. **Sync now** runs every pair straight away.

## Using it in an app

```rust
use std::sync::Arc;
use one_drive_sync::*;

let auth = Arc::new(Auth::new(CLIENT_ID, DriveScope::AppFolder,
    Box::new(KeyringStore::new("Waypoint", CLIENT_ID))));
// Once, from a button: opens the browser, waits for the redirect.
auth.sign_in(&mut |url| open::that(url), &cancel)?;

let drive = GraphDrive::new(auth.clone(), DriveScope::AppFolder);
let service = SyncService::start(Box::new(drive), state_dir, SyncOptions::default(),
    Box::new(move || ctx.request_repaint()));
service.set_pairs(vec![PairConfig::new(trips_dir, "Waypoint/trips", Mode::TwoWay)]);
// Each frame:
for event in service.events() { /* Event::Log, Event::Status */ }
```

`PairConfig` and `Rules` are serde types, so an app can save them with its settings.

## Trying it

### 1. Register the app in Azure (once)

1. Go to <https://entra.microsoft.com> (or portal.azure.com) → **App registrations** → **New registration**.
2. **Name:** this becomes the OneDrive folder `Apps/<name>/`, for example "Timster Sync".
3. **Supported account types:** *Personal Microsoft accounts only*.
4. **Redirect URI:** platform *Public client/native (mobile & desktop)*, value `http://localhost`. Any port is accepted, and the agent picks a free one.
5. Under **API permissions**, add the delegated Microsoft Graph permissions `Files.ReadWrite.AppFolder` (or `Files.ReadWrite` for the whole drive) and `offline_access`.
6. Copy the **Application (client) ID**. No secret is needed.

### 2. Run the agent

```sh
cargo run --release -p odsync-agent
```

For Windows, CI builds `odsync-agent.exe`; it is also available locally with `cargo build --release -p odsync-agent --target x86_64-pc-windows-gnu`.

Paste the client id under **Settings** (or set `ODSYNC_CLIENT_ID`), click **Apply**, then **Sign in**. Settings and pair state are kept in `~/.local/share/onedrive-sync-agent/` on Linux and `%APPDATA%\OneDrive Sync Agent\` on Windows.

On Linux the token needs a Secret Service (GNOME Keyring or KWallet). Without one, signing in fails with a message about the keychain.

### 3. The trial plan

- A **push pair**: the weather folder → `weather`. Check that new, changed and deleted files appear on OneDrive, and that nothing comes down.
- A **two-way pair**: a *copy* of a Waypoint trip folder → `Waypoint/trips`, on both the Linux and the Windows machine. Try:
  - an edit on each side, synced one after the other;
  - the same trip edited on both machines before either syncs (expect a `(conflict from HOST)` copy on both);
  - taking and releasing a `.trip.lock` from each side;
  - deleting a trip on one side;
  - unplugging the network, making changes, then reconnecting.
- Look in OneDrive's recycle bin for the deleted files.

## Not in the trial

Upload sessions (files over 4 MB), a tray icon, autostart, installers, Waypoint integration and the iOS app are all left out. Two OneDrive limits also apply: names that differ only in case aren't told apart (OneDrive is case-insensitive), and files whose names OneDrive refuses won't upload.

## Checks before pushing

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --target x86_64-pc-windows-gnu -- -D warnings
```
