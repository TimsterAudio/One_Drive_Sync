//! Folder sync with a personal OneDrive over Microsoft Graph.
//!
//! - [`auth`]: sign-in (auth code + PKCE through the browser), refresh token
//!   in the OS keychain.
//! - [`graph`]: the drive over Microsoft Graph; [`fake`]: an in-memory drive
//!   for tests. Both implement [`RemoteDrive`].
//! - [`engine`]: one sync run of a pair (a local folder, a OneDrive folder,
//!   `Push` or `TwoWay`), settled by per-pattern [`rules`].
//! - [`worker`]: a thread that runs the pairs on file changes, on a poll
//!   interval and on request.
//!
//! All I/O is blocking and meant for a worker thread. Files are copied as
//! bytes; their contents are never parsed or changed.

pub mod auth;
pub mod engine;
pub mod fake;
pub mod graph;
pub mod local;
pub mod quickxor;
pub mod remote;
pub mod rules;
pub mod state;
pub mod worker;

pub use auth::{Auth, AuthError, KeyringStore, MemoryStore, TokenStore};
pub use engine::{sync, Level, LogLine, Mode, PairConfig, Report, SyncError, SyncOptions};
pub use fake::FakeDrive;
pub use graph::{DriveScope, GraphDrive, TokenSource};
pub use remote::{Precondition, RemoteDrive, RemoteError};
pub use rules::{BothChanged, DeleteVsEdit, Rule, Rules};
pub use worker::{Event, PairStatus, SyncService};
