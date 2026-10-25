# Changelog

Apps pin a commit; each version below names its commit once pushed.

## Unreleased

## 0.1.0 – 2026-10-25 (trial) – commit 413f985

First version, for the trial agent.

- Sign-in with a personal Microsoft account: auth code + PKCE through the browser and a `http://localhost` loopback; refresh token in the OS keychain (split across entries if it's over Windows' 2560-byte limit).
- `GraphDrive`: delta queries with the delta link saved, downloads, simple uploads (up to 4 MB) conditional on the eTag, deletes to the recycle bin. App folder or whole drive.
- The sync engine: `Push` and `TwoWay` pairs, a state file per pair, per-pattern rules (`*.trip.json` keeps both, `*.trip.lock` newest wins and deletions win, `*.html` newest wins), and a guard against emptying a side.
- `FakeDrive` and engine tests: both sides changed, delete versus edit, lock churn between two computers, offline then reconnect, remote folder moves and deletes, forced resync.
- `SyncService`: a worker thread with a file watcher, a poll interval per pair, and events for the UI.
- `odsync-agent`: the trial window (sign in, add folder pair, sync now, status and log).
