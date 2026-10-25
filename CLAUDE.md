# One_Drive_Sync: working conventions

A shared crate that keeps local folders in sync with another folder (a mounted LAN share, via `FolderDrive`) or, behind the `onedrive` feature, a personal OneDrive. `agent/` is Folder Sync, the SyncToy-like app that uses it on Linux. README.md says how it works.

## Hard rules

- **Never parse or alter file contents.** Files are copied as bytes and compared by hash. Nothing in the engine may depend on what's inside a file.
- **No app logic.** Anything only one app needs (its folders, its UI) stays in that app. The default rules are the one exception: they encode Waypoint's file patterns, and an app can replace them.
- **The engine is tested against `FakeDrive` and `FolderDrive` on temp folders, never the network.** A new remote behaviour goes into the fake first, then into a test. `cargo test` must never need a network, a mounted share or a keychain.
- **Losing data is the worst outcome.** When unsure, keep both copies, defer to the next run, or stop with a message. Never overwrite a remote change without `If-Match` (except in Push mode, which says so in the log).
- **Apps pin a commit.** Each version is a commit recorded in CHANGELOG.md; never rewrite history on a branch apps pin. A changed signature or state-file meaning is a new minor version (0.x.0), with a note on what callers do; fixes are a patch. State and config types use `#[serde(default)]`: add fields with defaults, never rename one.

## Checks before pushing

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo clippy --workspace --target x86_64-pc-windows-gnu -- -D warnings
```

For anything visible in the agent, run it under Xvfb in a cloud session and look at it.
