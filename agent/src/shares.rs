//! Mounting Windows shares without root, through GVFS (what the file
//! manager's "Connect to Server" uses): `gio mount smb://PC/share`, with the
//! password kept in the system keyring. A mounted share shows up as a folder
//! under `/run/user/<uid>/gvfs/`, which pairs then sync with.
//!
//! A thread keeps the shares mounted: it checks them every minute and
//! remounts any that dropped (the PC slept, the network blinked). A share
//! whose login was refused isn't retried until the password is entered again,
//! so a stale password can't lock the Windows account.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

const KEYRING_SERVICE: &str = "Folder Sync";
const CHECK_EVERY: Duration = Duration::from_secs(60);
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
    pub fn uri(&self) -> String {
        format!("smb://{}/{}/", self.server.trim(), self.name.trim())
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
        .unwrap_or_else(|| "gio couldn't mount it".into());
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

/// Mounts the share, answering gio's prompts for the user, domain and
/// password on its standard input (never on its command line).
fn mount(share: &Share, password: &str) -> MountState {
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
        let _ = write!(
            stdin,
            "{}\n{}\n{password}\n",
            share.user.trim(),
            share.domain.trim()
        );
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
        Some(s) if s.success() => match local_path(share) {
            Some(p) => MountState::Mounted(p),
            None => MountState::Failed {
                message: "mounted, but GVFS gave it no folder (is gvfs-fuse installed?)".into(),
                bad_login: false,
            },
        },
        Some(_) => failure(&stdout, &stderr),
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

/// The keyring entry holding a share's password.
fn keyring_entry(share: &Share) -> Option<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, &share.key()).ok()
}

pub fn forget_password(share: &Share) {
    if let Some(e) = keyring_entry(share) {
        let _ = e.delete_credential();
    }
}

enum Cmd {
    SetShares(Vec<Share>),
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
    pub fn start(wake: Box<dyn Fn() + Send>) -> Self {
        let (tx, rx) = mpsc::channel();
        let (etx, events) = mpsc::channel();
        std::thread::Builder::new()
            .name("mounter".into())
            .spawn(move || run(rx, etx, wake))
            .expect("couldn't start the mount thread");
        Self { tx, events }
    }

    pub fn set_shares(&self, shares: Vec<Share>) {
        let _ = self.tx.send(Cmd::SetShares(shares));
    }

    pub fn mount(&self, key: &str, password: Option<String>) {
        let _ = self.tx.send(Cmd::Mount(key.to_string(), password));
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
}

fn run(rx: Receiver<Cmd>, events: Sender<MountEvent>, wake: Box<dyn Fn() + Send>) {
    let mut shares: Vec<Tracked> = Vec::new();
    // Passwords for this session when the keyring can't keep them.
    let mut session: HashMap<String, String> = HashMap::new();
    let send = |t: &Tracked, note: Option<String>| {
        let _ = events.send(MountEvent {
            key: t.share.key(),
            state: t.state.clone(),
            note,
        });
        wake();
    };

    let password_for = |share: &Share, session: &HashMap<String, String>| {
        session
            .get(&share.key())
            .cloned()
            .or_else(|| keyring_entry(share)?.get_password().ok())
    };

    let ensure = |t: &mut Tracked, session: &HashMap<String, String>, forced: bool| {
        if let MountState::Mounted(p) = &t.state {
            if p.is_dir() {
                return None;
            }
        }
        if let Some(p) = local_path(&t.share) {
            t.state = MountState::Mounted(p);
            return Some(None);
        }
        let blocked = matches!(
            t.state,
            MountState::Failed {
                bad_login: true,
                ..
            }
        );
        if !forced && (t.held || blocked) {
            return None;
        }
        let Some(password) = password_for(&t.share, session) else {
            t.state = MountState::Failed {
                message: "no password saved; enter it to connect".into(),
                bad_login: true,
            };
            return Some(None);
        };
        Some(Some(password))
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
                            share,
                        }
                    })
                    .collect();
            }
            Some(Cmd::Mount(key, password)) => {
                if let Some(t) = shares.iter_mut().find(|t| t.share.key() == key) {
                    t.held = false;
                    if let Some(p) = password {
                        let saved = keyring_entry(&t.share)
                            .map(|e| e.set_password(&p).is_ok())
                            .unwrap_or(false);
                        if saved {
                            session.remove(&key);
                        } else {
                            session.insert(key.clone(), p);
                            send(
                                t,
                                Some(format!(
                                    "{}: couldn't save the password to the keyring; it's kept until Folder Sync closes.",
                                    t.share.label()
                                )),
                            );
                        }
                    }
                    forced = Some(key);
                }
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
            match ensure(t, &session, is_forced) {
                None => {}
                Some(None) => {
                    if t.state != before {
                        send(t, None);
                    }
                }
                Some(Some(password)) => {
                    t.state = MountState::Mounting;
                    send(t, None);
                    t.state = mount(&t.share, &password);
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
        next_check = Instant::now() + CHECK_EVERY;
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
    fn names() {
        let s = Share {
            server: "ATLAS".into(),
            name: "trips".into(),
            user: "me@outlook.com".into(),
            ..Default::default()
        };
        assert_eq!(s.uri(), "smb://ATLAS/trips/");
        assert_eq!(s.label(), "\\\\ATLAS\\trips");
        assert_eq!(s.key(), "smb://me@outlook.com@ATLAS/trips");
    }
}
