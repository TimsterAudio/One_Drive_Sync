//! Two-way folder sync: a local folder kept in step with another folder (a
//! mounted LAN share) or, with the `onedrive` feature, a personal OneDrive.
//!
//! - [`engine`]: one sync run of a pair (a local folder, the other side,
//!   `Push` or `TwoWay`), settled by per-pattern [`rules`].
//! - [`folder`]: the other side as a folder; [`fake`]: an in-memory drive for
//!   tests. Both implement [`RemoteDrive`].
//! - [`worker`]: a thread that runs the pairs on file changes, on a poll
//!   interval and on request.
//! - `auth` and `graph` (feature `onedrive`): sign-in and the drive over
//!   Microsoft Graph.
//!
//! All I/O is blocking and meant for a worker thread. Files are copied as
//! bytes; their contents are never parsed or changed.

#[cfg(feature = "onedrive")]
pub mod auth;
pub mod engine;
pub mod fake;
pub mod folder;
#[cfg(feature = "onedrive")]
pub mod graph;
pub mod local;
pub mod quickxor;
pub mod remote;
pub mod rules;
pub mod state;
pub mod worker;

#[cfg(feature = "onedrive")]
pub use auth::{Auth, AuthError, KeyringStore, MemoryStore, TokenStore};
pub use engine::{sync, Level, LogLine, Mode, PairConfig, Report, SyncError, SyncOptions};
pub use fake::FakeDrive;
pub use folder::FolderDrive;
#[cfg(feature = "onedrive")]
pub use graph::{DriveScope, GraphDrive, TokenSource};
pub use remote::{Precondition, RemoteDrive, RemoteError};
pub use rules::{BothChanged, DeleteVsEdit, Rule, Rules};
pub use worker::{Event, PairStatus, SyncService};
