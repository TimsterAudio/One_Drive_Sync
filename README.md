# One_Drive_Sync

A small Rust crate and app that keep folders on two computers in step, like SyncToy. Run **Folder Sync** on the Linux machine, and it connects to the Windows PC's shared folders over the LAN and syncs with them. The Windows PC needs nothing installed. If those folders sit inside the PC's OneDrive folder, OneDrive's own app carries them on to the cloud and the iPhone.

The crate started as a OneDrive client. That code is still here, behind the `onedrive` feature, for when an app needs to talk to OneDrive directly; see [OneDrive](#onedrive-parked). Folder Sync doesn't build it, so it needs no Azure registration or Microsoft sign-in.

**Status: 0.3.0, trial.** It works end to end in the tests and against a Samba share (standing in for the PC) connected through GVFS. It still needs a try with the real Windows PC (see [Trying it](#trying-it)).

## How it fits together

```
Linux                            Windows PC
~/Waypoint/trips  ⇄ Folder Sync ⇄  \\PC\trips  (C:\Users\you\OneDrive\Waypoint\trips)
~/weather         → Folder Sync →  \\PC\weather                  │
                                                  OneDrive app ──► cloud, iPhone
```

## Trying it

### 1. Share the folders on the Windows PC

For each folder (a Waypoint trip folder, the weather folder):

1. Right-click the folder, then choose **Properties → Sharing → Advanced Sharing → Share this folder**.
2. Under **Permissions**, give your Windows user **Change** and **Read**.
3. Note the share name, for example `trips`. Its address is `\\PC-NAME\trips`.

If the folders are inside the PC's OneDrive folder, right-click them in Explorer and choose **Always keep on this device**. That makes sure the share holds real files, not online-only placeholders.

### 2. Connect the shares from Folder Sync

Run Folder Sync (next step). Under **Shares on other computers**, enter the PC's name (or its IP address), the share name, your Windows user name and your password, then click **Add and connect**. With a Microsoft account login on Windows, the user name is the account's email address and the password is that account's password.

Folder Sync connects through GVFS, the same service the file manager uses for **Connect to Server**, so it needs no root and no command line. On Mint it's already installed; elsewhere, install `gvfs-backends` and `gvfs-fuse`. A connected share appears as a folder under `/run/user/1000/gvfs/`, and the **Shares…** button in the pair form fills that folder in.

- **The password** is kept in the system keyring (Passwords and Keys, unlocked when you log in). If the keyring can't take it, Folder Sync keeps it only until it closes and says so in the log.
- **Shares reconnect by themselves:** at start, and within a minute of dropping (the PC slept, the network blinked). Meanwhile the pairs on that share wait, and nothing is deleted.
- **A refused password is not retried.** That way a stale password can't lock your Windows account. The share asks for the password again instead.
- **Disconnect** keeps a share disconnected until you click **Connect**. **Remove** disconnects it and forgets its password.

If you'd rather mount the shares yourself (`cifs` in `/etc/fstab`, say), skip this step and point the pairs at those folders.

### 3. Run Folder Sync

```sh
cargo run --release -p folder-sync
```

CI also builds a Linux and a Windows copy on every push, under the run's artifacts.

Add a pair: **This computer** is the local folder, and **Other folder** is the share (**Shares…** fills in a connected one; use Browse… to pick a subfolder inside it). Choose the mode:

- **Two-way** for the trip folder.
- **Push** for the weather files: this computer's changes go across, deletions included. Copies changed on the PC are replaced, nothing comes back, and files on the PC that this pair never sent are left alone.

To have it start by itself, tick **Start when I log in** (top right). This adds an entry to `~/.config/autostart/`, which Linux Mint's Cinnamon (and GNOME, KDE, Xfce) reads at login. Folder Sync then opens minimised, 10 seconds after login, and connects its shares. If a share isn't reachable yet, its pairs wait and Folder Sync keeps trying. The entry points at the copy of `folder-sync` that ticked the box, so tick it from the copy you'll keep (say, `~/.local/bin/folder-sync`). Untick the box to remove the entry. You can also see and toggle it in Mint's **Startup Applications**.

Pairs and their state are kept in `~/.local/share/folder-sync/` (`%APPDATA%\Folder Sync\` on Windows). Removing a pair touches no files.

### 4. The trial plan

- **Push pair** (the weather folder): new, changed and deleted files show up on the PC, and nothing comes back.
- **Two-way pair** (a copy of a Waypoint trip folder). Try each of these:
  - an edit on each side;
  - the same trip edited on both sides before a sync (expect a `(conflict from HOST)` copy on both);
  - taking and releasing a `.trip.lock` from each side;
  - deleting a trip on one side;
  - turning the PC off (or unmounting the share), making changes, then bringing it back.
- Deleted files are recoverable: files deleted on the PC go to `.odsync-trash` in the share, and files deleted on Linux go to the wastebasket.

## How syncing works

A **pair** is a folder on this computer, the other folder, a mode and a poll interval.

**Change detection.** A watcher (`notify`) on the local folder triggers a sync once the folder has been quiet for 1.5 s. Changes made on the PC can't be watched over SMB, so each pair also checks the other folder on its own interval: every 20 s by default, more often if you like. **Sync now** runs every pair straight away.

**The state file** for each pair records, for every path, the content hash at the last sync plus each side's size and modification time. A file is re-hashed only when its size or time has moved, so a touched but unchanged file counts as unchanged. Hashes of the share's files are also kept in memory, so after the first run only changed files are read across the network. On the first sync, a file that already exists on both sides with the same content is simply adopted: no conflict, no copy. **File contents are never parsed or changed**; files are copied byte for byte. A file arrives under a temporary name (`*.odsync-tmp`) and is then renamed into place, so nobody ever sees it half-written.

### Rules

Rules are tried in order on the file name (`*` and `?` wildcards, case-insensitive), and the first match decides:

| Pattern | Both sides changed | One side deleted, the other edited |
|---|---|---|
| `*.trip.json` | **Keep both.** The other side's copy keeps the name; this computer's copy becomes `name (conflict from HOST).trip.json` and syncs too | The edit survives |
| `*.trip.lock` | **Newest wins** (by modification time) | **The deletion wins** (a released lock stays released) |
| `*.html` | **Newest wins** | The edit survives |
| anything else | Keep both | The edit survives |

"Newest wins" compares the two machines' clocks, so keep both set automatically. Both Windows and Linux do this by default.

Deletions sync both ways when the other side hasn't changed. Some files never sync: `*.odsync-tmp`, `.DS_Store`, `Thumbs.db`, `desktop.ini`, `~$*` and `.~lock.*`.

### Safety checks

- **A missing share stops the run.** If the share isn't mounted (the PC is off, the network is down), the pair shows the error and retries at the next check. Nothing is deleted.
- **An empty share stops the run.** If the share's folder is there but empty while files were synced, the run stops rather than delete everything here. An empty mount point usually means nothing is mounted. If you really did empty one side, empty the other side too.
- **Fresh files wait.** A file modified in the last 2 seconds waits for the next run, in case it is still being written. Any file that changes between the scan and the copy also waits.
- **Changes on the PC aren't overwritten.** A copy to the share checks first that the file there hasn't changed since the scan; if it has, the next run settles it by the rules. Push mode is the exception, and the log says so when it replaces a file.

## Using it in an app

```rust
use one_drive_sync::*;

let service = SyncService::start_folders(state_dir, SyncOptions::default(),
    Box::new(move || ctx.request_repaint()));
service.set_pairs(vec![PairConfig::folders(trips_dir, "/mnt/pc-trips", Mode::TwoWay)]);
// Each frame:
for event in service.events() { /* Event::Log, Event::Status */ }
```

```toml
one_drive_sync = { git = "https://github.com/TimsterAudio/One_Drive_Sync", rev = "<commit>", default-features = false }
```

`PairConfig` and `Rules` are serde types, so an app can save them with its settings. Under the hood, the engine talks to a `RemoteDrive`. `FolderDrive` is one for a folder; `FakeDrive` is an in-memory one for tests.

## OneDrive (parked)

With the default `onedrive` feature, the crate also syncs pairs directly with a personal OneDrive over Microsoft Graph (`GraphDrive`, `Auth`):

- **Sign-in:** auth code + PKCE in the browser, with the refresh token in the OS keychain.
- **Changes:** delta queries.
- **Uploads:** conditional on the eTag (`If-Match`).
- **Deletes:** to the recycle bin.

It needs an Azure app registration, with personal accounts, the `http://localhost` redirect, and `Files.ReadWrite.AppFolder` + `offline_access`. That registration is the step that made it heavy for now. Start such pairs with `SyncService::start(Box::new(GraphDrive::new(...)), ...)`.

## Not in the trial

- A tray icon and installers. Autostart on Windows (Linux has it, above).
- Names that differ only in case aren't told apart, because Windows doesn't tell them apart either.

## Checks before pushing

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --target x86_64-pc-windows-gnu -- -D warnings
```
