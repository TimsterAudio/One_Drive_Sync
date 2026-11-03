//! Mounting Windows shares without root, through GVFS (what the file
//! manager's "Connect to Server" uses): `gio mount smb://PC/share`, with the
//! password kept in the system keyring. A mounted share shows up as a folder
//! under `/run/user/<uid>/gvfs/`, which pairs then sync with.
//!
//! A thread keeps the shares mounted: it checks them every minute, and
//! retries one that's down every 15 s (at login, until the network and the
//! keyring are up; later, after the PC slept or the network blinked). A share
//! whose password was refused isn't retried until the password is entered
//! again, so a stale password can't lock the Windows account.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use keyring_core::api::CredentialStore;
use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "Folder Sync";
/// How often mounted shares are checked.
const CHECK_EVERY: Duration = Duration::from_secs(60);
/// How often a share that's down is retried (after login, the network and
/// the keyring can take a while).
const RETRY_EVERY: Duration = Duration::from_secs(15);
const MOUNT_TIMEOUT: Duration = Duration::from_secs(45);

/// A share on another computer: `\\server\name`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Share {
    /// The computer's name or address.
    pub server: String,
    /// The share's name.
    pub name: String,
    /// The Windows user (the email address for a Microsoft account).
    pub user: String,
    pub domain: String,
}

impl Default for Share {
    fn default() -> Self {
        Self {
            server: String::new(),
            name: String::new(),
            user: String::new(),
            domain: "WORKGROUP".into(),
        }
    }
}

impl Share {
    /// `smb://domain;user@server/name/`. With the domain and user in it,
    /// GVFS names the mount folder just as the file manager does
    /// (`smb-share:domain=…,server=…,share=…,user=…`), so a pair set up on a
    /// share the file manager connected still finds it.
    pub fn uri(&self) -> String {
        let mut who = String::new();
        if !self.user.trim().is_empty() {
            if !self.domain.trim().is_empty() {
                who.push_str(&encode_userinfo(self.domain.trim()));
                who.push(';');
            }
            who.push_str(&encode_userinfo(self.user.trim()));
            who.push('@');
        }
        format!("smb://{who}{}/{}/", self.server.trim(), self.name.trim())
    }

    /// The share a GVFS folder belongs to, read from its
    /// `smb-share:domain=…,server=…,share=…,user=…` part.
    pub fn from_gvfs_path(path: &Path) -> Option<Share> {
        let spec = path
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .find_map(|c| c.strip_prefix("smb-share:"))?;
        let mut share = Share {
            domain: String::new(),
            ..Default::default()
        };
        for kv in spec.split(',') {
            let (k, v) = kv.split_once('=')?;
            let v = percent_decode(v);
            match k {
                "server" => share.server = v,
                "share" => share.name = v,
                "user" => share.user = v,
                "domain" => share.domain = v,
                _ => {}
            }
        }
        (!share.server.is_empty() && !share.name.is_empty()).then_some(share)
    }

    /// Identifies the share in events and the keyring.
    pub fn key(&self) -> String {
        format!(
            "smb://{}@{}/{}",
            self.user.trim(),
            self.server.trim(),
            self.name.trim()
        )
    }

    /// `\\server\name`, as Windows writes it.
    pub fn label(&self) -> String {
        format!("\\\\{}\\{}", self.server.trim(), self.name.trim())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MountState {
    Unknown,
    Mounting,
    Mounted(PathBuf),
    /// Not mounted; `bad_login` means the user name or password was refused,
    /// and it won't be retried until the password is entered again.
    Failed {
        message: String,
        bad_login: bool,
    },
}

/// Percent-encodes a user or domain name for an smb:// address.
fn encode_userinfo(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'.'
            | b'_'
            | b'~'
            | b'!'
            | b'$'
            | b'&'
            | b'\''
            | b'('
            | b')'
            | b'*'
            | b'+'
            | b','
            | b'=' => out.push(b as char),
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether `gio` (GVFS) is here to mount with.
pub fn available() -> bool {
    !cfg!(windows)
        && Command::new("gio")
            .arg("version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

fn gio() -> Command {
    let mut c = Command::new("gio");
    // English messages, which `mount` below reads.
    c.env("LC_ALL", "C").env("LANGUAGE", "C");
    c
}

/// Where the share is mounted, if it is.
fn local_path(share: &Share) -> Option<PathBuf> {
    let out = gio()
        .args(["info", &share.uri()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("local path: "))
        .map(|p| PathBuf::from(p.trim()))
        .filter(|p| p.is_dir())
}

/// Reads what gio said about a failed mount.
fn failure(stdout: &str, stderr: &str) -> MountState {
    // A refused login makes gio ask a second time, then give up.
    if stdout.matches("Authentication Required").count() > 1 {
        return MountState::Failed {
            message: "the user name or password was refused".into(),
            bad_login: true,
        };
    }
    let detail = stderr
        .lines()
        .filter(|l| !l.contains("WARNING"))
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| {
            let l = l.trim();
            // "gio: smb://pc/x/: Failed to mount Windows share: Connection timed out"
            l.rsplit_once(": Failed to mount Windows share: ")
                .map(|(_, why)| why)
                .or_else(|| l.rsplit_once("/: ").map(|(_, why)| why))
                .unwrap_or(l)
                .to_string()
        })
        .unwrap_or_default();
    if detail.is_empty() && stdout.contains("Authentication Required") {
        // Asked once and got no answer: no password was given or saved.
        return MountState::Failed {
            message: "enter the password to connect".into(),
            bad_login: true,
        };
    }
    let detail = if detail.is_empty() {
        "gio couldn't mount it".to_string()
    } else {
        detail
    };
    let message = match detail.as_str() {
        "Connection timed out" | "No route to host" | "Connection refused" => {
            format!("{detail} (is the PC on and sharing?)")
        }
        "No such file or directory" => "no share by that name on that computer".into(),
        "Permission denied" => {
            return MountState::Failed {
                message: "permission denied (check the user name and password)".into(),
                bad_login: true,
            }
        }
        _ => detail,
    };
    MountState::Failed {
        message,
        bad_login: false,
    }
}

/// Mounts the share, answering gio's password prompt on its standard input
/// (never on its command line). Without a password, GVFS can still use one
/// the file manager saved ("Remember forever").
fn mount(share: &Share, password: Option<&str>) -> MountState {
    let child = gio()
        .args(["mount", &share.uri()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return MountState::Failed {
                message: format!("couldn't run gio: {e}"),
                bad_login: false,
            }
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(password) = password {
            let _ = writeln!(stdin, "{password}");
        }
    }
    // Read the output on threads so a chatty gio can't block on a full pipe.
    let reader = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_string(&mut s);
            }
            s
        })
    };
    let out = reader(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let err = reader(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );

    let deadline = Instant::now() + MOUNT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    match status {
        None => MountState::Failed {
            message: "timed out (is the PC on?)".into(),
            bad_login: false,
        },
        Some(s) if s.success() => match wait_for_folder(share) {
            Some(p) => MountState::Mounted(p),
            None => MountState::Failed {
                message: "mounted, but GVFS gave it no folder (is gvfs-fuse installed?)".into(),
                bad_login: false,
            },
        },
        Some(_) => failure(&stdout, &stderr),
    }
}

/// The mount's folder, which GVFS can show a moment after the mount itself.
fn wait_for_folder(share: &Share) -> Option<PathBuf> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(p) = local_path(share) {
            return Some(p);
        }
        if Instant::now() > deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn unmount(share: &Share) {
    let _ = gio()
        .args(["mount", "-u", &share.uri()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// The system keyring, reached afresh each time: a connection kept from
/// before the keyring was up (or unlocked) at login can stay broken.
fn keyring() -> Option<Arc<CredentialStore>> {
    connect_keyring()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn connect_keyring() -> Option<Arc<CredentialStore>> {
    let store = zbus_secret_service_keyring_store::Store::new().ok()?;
    Some(store)
}

#[cfg(windows)]
fn connect_keyring() -> Option<Arc<CredentialStore>> {
    let store = windows_native_keyring_store::Store::new().ok()?;
    Some(store)
}

#[cfg(not(any(windows, all(unix, not(target_os = "macos")))))]
fn connect_keyring() -> Option<Arc<CredentialStore>> {
    None
}

/// The keyring entry holding a share's password.
fn keyring_entry(share: &Share) -> Option<keyring_core::Entry> {
    keyring()?.build(KEYRING_SERVICE, &share.key(), None).ok()
}

/// Where a password was saved.
#[derive(Debug, PartialEq, Eq)]
enum Saved {
    Keyring,
    /// In the passwords file, readable only by this user.
    File,
    /// Only until Folder Sync closes.
    Session,
}

/// Share passwords: in the system keyring, or, where there's no keyring to
/// be had, in a file only this user can read (like `~/.smbcredentials`).
struct Passwords {
    file: PathBuf,
    session: HashMap<String, String>,
}

impl Passwords {
    fn read_file(&self) -> HashMap<String, String> {
        std::fs::read(&self.file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write_file(&self, map: &HashMap<String, String>) -> std::io::Result<()> {
        if map.is_empty() {
            return match std::fs::remove_file(&self.file) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            };
        }
        if let Some(dir) = self.file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let bytes = serde_json::to_vec_pretty(map).map_err(std::io::Error::other)?;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&self.file)?.write_all(&bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.file, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    fn get(&self, share: &Share) -> Option<String> {
        let key = share.key();
        self.session
            .get(&key)
            .cloned()
            .or_else(|| keyring_entry(share)?.get_password().ok())
            .or_else(|| self.read_file().remove(&key))
    }

    fn set(&mut self, share: &Share, password: String) -> Saved {
        let key = share.key();
        let in_keyring = keyring_entry(share)
            .map(|e| e.set_password(&password).is_ok())
            .unwrap_or(false);
        let mut file = self.read_file();
        if in_keyring {
            self.session.remove(&key);
            if file.remove(&key).is_some() {
                let _ = self.write_file(&file);
            }
            return Saved::Keyring;
        }
        file.insert(key.clone(), password.clone());
        if self.write_file(&file).is_ok() {
            self.session.remove(&key);
            Saved::File
        } else {
            self.session.insert(key, password);
            Saved::Session
        }
    }

    fn forget(&mut self, share: &Share) {
        let key = share.key();
        self.session.remove(&key);
        if let Some(e) = keyring_entry(share) {
            let _ = e.delete_credential();
        }
        let mut file = self.read_file();
        if file.remove(&key).is_some() {
            let _ = self.write_file(&file);
        }
    }
}

enum Cmd {
    SetShares(Vec<Share>),
    /// Forget a removed share's password.
    Forget(Share),
    /// Mount now, with a newly entered password if given.
    Mount(String, Option<String>),
    Unmount(String),
}

/// A share's state changed.
pub struct MountEvent {
    pub key: String,
    pub state: MountState,
    /// A note for the log, if any.
    pub note: Option<String>,
}

/// The handle the app keeps; dropping it stops the thread (shares stay
/// mounted, as they would from the file manager).
pub struct Mounter {
    tx: Sender<Cmd>,
    events: Receiver<MountEvent>,
}

impl Mounter {
    /// `password_file` is where passwords go if the keyring can't take them.
    pub fn start(password_file: PathBuf, wake: Box<dyn Fn() + Send>) -> Self {
        let (tx, rx) = mpsc::channel();
        let (etx, events) = mpsc::channel();
        let passwords = Passwords {
            file: password_file,
            session: HashMap::new(),
        };
        std::thread::Builder::new()
            .name("mounter".into())
            .spawn(move || run(rx, etx, wake, passwords))
            .expect("couldn't start the mount thread");
        Self { tx, events }
    }

    pub fn set_shares(&self, shares: Vec<Share>) {
        let _ = self.tx.send(Cmd::SetShares(shares));
    }

    pub fn mount(&self, key: &str, password: Option<String>) {
        let _ = self.tx.send(Cmd::Mount(key.to_string(), password));
    }

    /// Disconnects the share and forgets its password.
    pub fn remove(&self, share: &Share) {
        let _ = self.tx.send(Cmd::Unmount(share.key()));
        let _ = self.tx.send(Cmd::Forget(share.clone()));
    }

    pub fn unmount(&self, key: &str) {
        let _ = self.tx.send(Cmd::Unmount(key.to_string()));
    }

    pub fn events(&self) -> Vec<MountEvent> {
        self.events.try_iter().collect()
    }
}

struct Tracked {
    share: Share,
    state: MountState,
    /// Unmounted on request: not remounted until asked.
    held: bool,
    /// A password Folder Sync sent was refused: not retried until a new one
    /// is entered, so it can't lock the Windows account. A failure with no
    /// password sent (none saved yet, or the keyring not unlocked yet at
    /// login) doesn't count.
    refused: bool,
}

fn run(
    rx: Receiver<Cmd>,
    events: Sender<MountEvent>,
    wake: Box<dyn Fn() + Send>,
    mut passwords: Passwords,
) {
    let mut shares: Vec<Tracked> = Vec::new();
    let send = |t: &Tracked, note: Option<String>| {
        let _ = events.send(MountEvent {
            key: t.share.key(),
            state: t.state.clone(),
            note,
        });
        wake();
    };

    let ensure = |t: &mut Tracked, passwords: &Passwords, forced: bool| {
        if let MountState::Mounted(p) = &t.state {
            if p.is_dir() {
                return None;
            }
        }
        if let Some(p) = local_path(&t.share) {
            t.state = MountState::Mounted(p);
            return Some(None);
        }
        if !forced && (t.held || t.refused) {
            return None;
        }
        // No password of ours: try anyway, in case GVFS has one saved.
        Some(Some(passwords.get(&t.share)))
    };

    let mut next_check = Instant::now();
    loop {
        let cmd = match rx.recv_timeout(next_check.saturating_duration_since(Instant::now())) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut forced: Option<String> = None;
        match cmd {
            Some(Cmd::SetShares(list)) => {
                shares = list
                    .into_iter()
                    .map(|share| {
                        let old = shares.iter().find(|t| t.share == share);
                        Tracked {
                            state: old.map_or(MountState::Unknown, |t| t.state.clone()),
                            held: old.is_some_and(|t| t.held),
                            refused: old.is_some_and(|t| t.refused),
                            share,
                        }
                    })
                    .collect();
            }
            Some(Cmd::Mount(key, password)) => {
                if let Some(t) = shares.iter_mut().find(|t| t.share.key() == key) {
                    t.held = false;
                    t.refused = false;
                    if let Some(p) = password {
                        let note = match passwords.set(&t.share, p) {
                            Saved::Keyring => "password saved in the keyring".to_string(),
                            Saved::File => format!(
                                "no keyring to save the password in, so it's saved in {} (only you can read it)",
                                passwords.file.display()
                            ),
                            Saved::Session => {
                                "couldn't save the password; it's kept until Folder Sync closes"
                                    .to_string()
                            }
                        };
                        send(t, Some(format!("{}: {note}.", t.share.label())));
                    }
                    forced = Some(key);
                }
            }
            Some(Cmd::Forget(share)) => {
                passwords.forget(&share);
                continue;
            }
            Some(Cmd::Unmount(key)) => {
                if let Some(t) = shares.iter_mut().find(|t| t.share.key() == key) {
                    unmount(&t.share);
                    t.held = true;
                    t.state = MountState::Failed {
                        message: "disconnected".into(),
                        bad_login: false,
                    };
                    send(t, Some(format!("{}: disconnected.", t.share.label())));
                }
                continue;
            }
            None => {}
        }

        for t in shares.iter_mut() {
            let is_forced = forced.as_deref() == Some(t.share.key().as_str());
            let before = t.state.clone();
            match ensure(t, &passwords, is_forced) {
                None => {}
                Some(None) => {
                    if t.state != before {
                        send(t, None);
                    }
                }
                Some(Some(password)) => {
                    t.state = MountState::Mounting;
                    send(t, None);
                    t.state = mount(&t.share, password.as_deref());
                    t.refused = password.is_some()
                        && matches!(
                            t.state,
                            MountState::Failed {
                                bad_login: true,
                                ..
                            }
                        );
                    let note = match &t.state {
                        MountState::Mounted(p) => Some(format!(
                            "{}: connected at {}.",
                            t.share.label(),
                            p.display()
                        )),
                        MountState::Failed { message, .. }
                            if *message != failed_message(&before) =>
                        {
                            Some(format!("{}: couldn't connect: {message}.", t.share.label()))
                        }
                        _ => None,
                    };
                    send(t, note);
                }
            }
        }
        let waiting = shares
            .iter()
            .any(|t| !t.held && !t.refused && !matches!(t.state, MountState::Mounted(_)));
        next_check = Instant::now() + if waiting { RETRY_EVERY } else { CHECK_EVERY };
    }
}

fn failed_message(state: &MountState) -> String {
    match state {
        MountState::Failed { message, .. } => message.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(state: MountState) -> (String, bool) {
        match state {
            MountState::Failed { message, bad_login } => (message, bad_login),
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn reads_gio_failures() {
        // A refused login: gio asks twice, then gives up quietly.
        let out = "Authentication Required\nEnter user and password for share 'trips' on 'pc':\n\
                   User [guest]: Domain [WORKGROUP]: Password: \nAuthentication Required\n\
                   Enter user and password for share 'trips' on 'pc':\nUser [guest]: \n";
        assert!(message(failure(out, "")).1);

        let (m, bad) = message(failure(
            "Authentication Required\n",
            "gio: smb://10.0.0.9/trips/: Failed to mount Windows share: Connection timed out\n",
        ));
        assert_eq!(m, "Connection timed out (is the PC on and sharing?)");
        assert!(!bad);

        let (m, _) = message(failure(
            "",
            "gio: smb://pc/nope/: Failed to mount Windows share: No such file or directory\n",
        ));
        assert_eq!(m, "no share by that name on that computer");
    }

    #[test]
    fn asked_once_means_no_password() {
        let out =
            "Authentication Required\nEnter password for share 'trips' on 'pc':\nPassword: \n";
        let (m, bad) = message(failure(out, ""));
        assert_eq!(m, "enter the password to connect");
        assert!(bad);
    }

    #[test]
    fn shares_from_file_manager_folders() {
        let p = Path::new(
            "/run/user/1000/gvfs/smb-share:domain=Home,server=192.168.0.2,share=documents,user=Tim%20&%20Sarah/Waypoint Data",
        );
        let s = Share::from_gvfs_path(p).unwrap();
        assert_eq!(s.server, "192.168.0.2");
        assert_eq!(s.name, "documents");
        assert_eq!(s.user, "Tim & Sarah");
        assert_eq!(s.domain, "Home");
        // Mounting this address makes GVFS use that same folder.
        assert_eq!(s.uri(), "smb://Home;Tim%20&%20Sarah@192.168.0.2/documents/");
        assert!(Share::from_gvfs_path(Path::new("/mnt/pc/trips")).is_none());
    }

    #[test]
    fn password_file_fallback() {
        let dir = std::env::temp_dir().join(format!("fs-pw-{}", std::process::id()));
        let pw = Passwords {
            file: dir.join("passwords.json"),
            session: HashMap::new(),
        };
        let share = Share {
            server: "pc".into(),
            name: "x".into(),
            user: "me".into(),
            ..Default::default()
        };
        let mut map = HashMap::new();
        map.insert(share.key(), "secret".to_string());
        pw.write_file(&map).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&pw.file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(pw.read_file().get(&share.key()).unwrap(), "secret");
        pw.write_file(&HashMap::new()).unwrap();
        assert!(!pw.file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn names() {
        let s = Share {
            server: "ATLAS".into(),
            name: "trips".into(),
            user: "me@outlook.com".into(),
            ..Default::default()
        };
        assert_eq!(s.uri(), "smb://WORKGROUP;me%40outlook.com@ATLAS/trips/");
        assert_eq!(s.label(), "\\\\ATLAS\\trips");
        assert_eq!(s.key(), "smb://me@outlook.com@ATLAS/trips");
    }
}
