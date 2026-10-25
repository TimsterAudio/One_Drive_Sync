//! Starting Folder Sync on login, through an XDG autostart entry
//! (`~/.config/autostart/folder-sync.desktop`), which Cinnamon, GNOME, KDE
//! and Xfce all read. Not offered on Windows.

use std::io;
use std::path::{Path, PathBuf};

const FILE: &str = "folder-sync.desktop";

/// Passed by the autostart entry: start minimised.
pub const MINIMIZED_ARG: &str = "--minimized";

/// The autostart folder, or `None` where autostart isn't offered.
pub fn dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|c| c.join("autostart"))
}

pub fn is_on(dir: &Path) -> bool {
    dir.join(FILE).is_file()
}

/// Writes the entry to start `exe` on login, or removes it.
pub fn set(dir: &Path, on: bool, exe: &Path) -> io::Result<()> {
    let path = dir.join(FILE);
    if on {
        std::fs::create_dir_all(dir)?;
        std::fs::write(path, entry(exe))
    } else {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

/// Points an existing entry at `exe`, in case the program has moved.
pub fn refresh(dir: &Path, exe: &Path) -> io::Result<()> {
    let path = dir.join(FILE);
    match std::fs::read_to_string(&path) {
        Ok(current) if current != entry(exe) => std::fs::write(path, entry(exe)),
        _ => Ok(()),
    }
}

fn entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Folder Sync\n\
         Comment=Keeps folders in step with a share on another computer\n\
         Exec={} {MINIMIZED_ARG}\n\
         Terminal=false\n\
         X-GNOME-Autostart-enabled=true\n\
         X-GNOME-Autostart-Delay=10\n",
        quote_exec(&exe.to_string_lossy())
    )
}

/// Quotes a path for a desktop entry's `Exec` key: in double quotes, with
/// `"`, `` ` ``, `$` and `\` escaped, and `%` doubled.
fn quote_exec(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '%' => out.push_str("%%"),
            c => out.push(c),
        }
    }
    out.push('"');
    // The .desktop file format escapes backslashes once more.
    out.replace('\\', "\\\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting() {
        assert_eq!(quote_exec("/opt/folder-sync"), "\"/opt/folder-sync\"");
        assert_eq!(
            quote_exec("/home/me/My Apps/100% $x"),
            "\"/home/me/My Apps/100%% \\\\$x\""
        );
    }

    #[test]
    fn on_off_and_refresh() {
        let dir = std::env::temp_dir().join(format!("fs-autostart-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!is_on(&dir));
        set(&dir, true, Path::new("/old/folder-sync")).unwrap();
        assert!(is_on(&dir));
        refresh(&dir, Path::new("/new/folder-sync")).unwrap();
        let text = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert!(text.contains("Exec=\"/new/folder-sync\" --minimized\n"));
        set(&dir, false, Path::new("/new/folder-sync")).unwrap();
        assert!(!is_on(&dir));
        // Turning off twice is fine; refresh doesn't turn it back on.
        set(&dir, false, Path::new("/new/folder-sync")).unwrap();
        refresh(&dir, Path::new("/new/folder-sync")).unwrap();
        assert!(!is_on(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
