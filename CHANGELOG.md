# Changelog

Apps pin a commit; each version below names its commit once pushed.

## Unreleased

## 0.2.1 – 2026-10-25 – commit eadbe57

Folder Sync only; the crate is unchanged.

- **Start when I log in**: a checkbox that adds or removes an XDG autostart entry (`~/.config/autostart/folder-sync.desktop`, read by Cinnamon, GNOME, KDE and Xfce). It starts Folder Sync minimised, 10 s after login, and the entry follows the program if it moves. Not offered on Windows yet.
- `folder-sync --minimized` starts with the window minimised.

## 0.2.0 – 2026-10-25 (trial) – commit f0a76dd

De-scoped to folder sync over the LAN: the Linux machine syncs with folders on the Windows PC through a mounted share, and OneDrive's own app takes them on from there.

- `FolderDrive`: the other side of a pair can be a folder (a mounted SMB share). Deletions there go to `.odsync-trash` in the share; a missing share counts as offline; hashes are cached so only changed files are read across the network.
- `PairConfig` gains `share` (default `None`, so saved pairs load as before) and `PairConfig::folders(local, share, mode)`; `remote_name()` names the other side.
- `SyncService::start_folders(state_dir, options, wake)` starts the worker with no OneDrive; `start` is unchanged.
- `Delta` gains `complete`: a drive with no change journal lists everything each time. Callers that build a `Delta` set it to `false`.
- For folder pairs, an empty other side stops the run as soon as one file was synced (an empty mount point), not five.
- The OneDrive code (`auth`, `graph`) is behind the `onedrive` feature, on by default; `default-features = false` drops it and its dependencies.
- Log and report wording no longer assumes OneDrive ("copied there", "copied here").
- The agent is now **Folder Sync** (`folder-sync`): folder pairs only, no sign-in.

## 0.1.0 – 2026-10-25 (trial) – commit 413f985

First version, for the trial agent.

- Sign-in with a personal Microsoft account: auth code + PKCE through the browser and a `http://localhost` loopback; refresh token in the OS keychain (split across entries if it's over Windows' 2560-byte limit).
- `GraphDrive`: delta queries with the delta link saved, downloads, simple uploads (up to 4 MB) conditional on the eTag, deletes to the recycle bin. App folder or whole drive.
- The sync engine: `Push` and `TwoWay` pairs, a state file per pair, per-pattern rules (`*.trip.json` keeps both, `*.trip.lock` newest wins and deletions win, `*.html` newest wins), and a guard against emptying a side.
- `FakeDrive` and engine tests: both sides changed, delete versus edit, lock churn between two computers, offline then reconnect, remote folder moves and deletes, forced resync.
- `SyncService`: a worker thread with a file watcher, a poll interval per pair, and events for the UI.
- `odsync-agent`: the trial window (sign in, add folder pair, sync now, status and log).
