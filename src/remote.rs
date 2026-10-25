//! The remote side of a sync pair, behind a trait so the engine can run
//! against [`GraphDrive`](crate::graph::GraphDrive) or the in-memory
//! [`FakeDrive`](crate::fake::FakeDrive).

use std::fmt;
use std::time::Duration;

/// A file or folder on the drive, as a delta query or an upload reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteItem {
    pub id: String,
    /// The containing folder's id; `None` for the top of the drive.
    pub parent_id: Option<String>,
    pub name: String,
    pub is_folder: bool,
    pub etag: String,
    pub size: u64,
    /// Last modified, in milliseconds since the Unix epoch.
    pub mtime_ms: i64,
    /// The QuickXorHash (base64) OneDrive keeps for files, when it has one.
    pub hash: Option<String>,
}

/// One change from a delta query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteChange {
    /// The item was created or changed (or is listed by a fresh query).
    Upsert(RemoteItem),
    /// The item went away. A deleted folder takes everything under it, even
    /// when the children aren't listed.
    Deleted { id: String },
}

/// Every change since the cursor, with the cursor for next time.
#[derive(Clone, Debug, Default)]
pub struct Delta {
    pub changes: Vec<RemoteChange>,
    pub cursor: String,
    /// The changes list everything there is, so anything not listed is
    /// gone (a drive with no change journal, such as a folder).
    pub complete: bool,
}

/// What an upload expects to find at the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Precondition {
    /// Nothing there yet; fail with [`RemoteError::Conflict`] if there is.
    New,
    /// The file with this eTag; fail with [`RemoteError::Conflict`] if it changed.
    Match(String),
    /// Replace whatever is there.
    Overwrite,
}

#[derive(Debug)]
pub enum RemoteError {
    /// Someone else changed the item first (HTTP 409 or 412).
    Conflict,
    NotFound,
    /// The saved delta cursor is no longer valid; start again without one.
    ResyncRequired,
    /// Slow down: try again after this long.
    Throttled(Duration),
    /// Not signed in, or the sign-in has expired or been withdrawn.
    Auth(String),
    /// Couldn't reach the service (offline, DNS, timeout).
    Offline(String),
    /// Larger than the simple upload allows (4 MB).
    TooLarge(u64),
    Other(String),
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteError::Conflict => write!(f, "changed on OneDrive by someone else"),
            RemoteError::NotFound => write!(f, "not found on OneDrive"),
            RemoteError::ResyncRequired => write!(f, "OneDrive asked for a full resync"),
            RemoteError::Throttled(d) => write!(f, "OneDrive asked to wait {}s", d.as_secs()),
            RemoteError::Auth(m) => write!(f, "sign-in needed: {m}"),
            RemoteError::Offline(m) => write!(f, "offline: {m}"),
            RemoteError::TooLarge(n) => {
                write!(f, "{n} bytes is over the 4 MB the trial uploads")
            }
            RemoteError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for RemoteError {}

pub type RemoteResult<T> = Result<T, RemoteError>;

/// The largest file a simple upload takes; larger ones need an upload session.
pub const SIMPLE_UPLOAD_LIMIT: u64 = 4 * 1024 * 1024;

/// A drive the engine syncs with. Paths are relative to the drive's base
/// (the whole drive, or the app folder), use `/`, and have no leading slash.
pub trait RemoteDrive {
    /// The folder at `path`, created (with any missing parents) if needed.
    fn ensure_folder(&mut self, path: &str) -> RemoteResult<RemoteItem>;

    /// Every change under the folder since `cursor`, following all pages.
    /// Without a cursor, lists everything under it, the folder included.
    fn delta(&mut self, folder_id: &str, cursor: Option<&str>) -> RemoteResult<Delta>;

    fn download(&mut self, id: &str) -> RemoteResult<Vec<u8>>;

    /// Uploads `data` to the file at `path`, creating parent folders as needed.
    fn upload(&mut self, path: &str, data: &[u8], cond: Precondition) -> RemoteResult<RemoteItem>;

    /// Moves the item to the recycle bin. With an eTag, fails with
    /// [`RemoteError::Conflict`] if it has changed since.
    fn delete(&mut self, id: &str, etag: Option<&str>) -> RemoteResult<()>;
}
