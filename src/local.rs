//! The local folder: listing, reading, and replacing files safely.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::quickxor::QuickXorHash;
use crate::rules::Rules;

/// Suffix of the file a download is written to before it replaces the real one.
pub const TMP_SUFFIX: &str = ".odsync-tmp";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalFile {
    pub size: u64,
    pub mtime_ms: i64,
}

pub fn to_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(e) => -(e.duration().as_millis() as i64),
    }
}

pub fn now_ms() -> i64 {
    to_ms(SystemTime::now())
}

/// The full path of `rel` (a `/`-separated path) under `root`.
pub fn full_path(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    p.extend(rel.split('/'));
    p
}

pub fn stat(path: &Path) -> io::Result<Option<LocalFile>> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_file() => Ok(Some(LocalFile {
            size: m.len(),
            mtime_ms: to_ms(m.modified()?),
        })),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Every file under `root`, by relative path. Symbolic links and ignored
/// names are left out.
pub fn scan(root: &Path, rules: &Rules) -> io::Result<BTreeMap<String, LocalFile>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue; // OneDrive can't hold names that aren't Unicode.
            };
            let rel = format!("{prefix}{name}");
            if rules.is_ignored(&rel) {
                continue;
            }
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push((entry.path(), format!("{rel}/")));
            } else if ft.is_file() {
                if let Some(f) = stat(&entry.path())? {
                    out.insert(rel, f);
                }
            }
        }
    }
    Ok(out)
}

/// The QuickXorHash of a file.
pub fn hash_file(path: &Path) -> io::Result<String> {
    let mut f = fs::File::open(path)?;
    let mut h = QuickXorHash::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            return Ok(h.finish_base64());
        }
        h.update(&buf[..n]);
    }
}

/// Writes `data` beside `path` and then moves it into place, so the file is
/// never seen half-written. Returns the new file's size and time.
pub fn replace(path: &Path, data: &[u8]) -> io::Result<LocalFile> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(TMP_SUFFIX);
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, data)?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    stat(path)?.ok_or_else(|| io::Error::other("file vanished after writing"))
}

/// Deletes a local file, to the system's wastebasket when `to_trash` is set.
pub fn delete(path: &Path, to_trash: bool) -> io::Result<()> {
    if to_trash {
        trash::delete(path).map_err(io::Error::other)
    } else {
        fs::remove_file(path)
    }
}
