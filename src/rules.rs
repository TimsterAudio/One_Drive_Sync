//! Per-pattern rules: what to do when both sides changed a file.

use serde::{Deserialize, Serialize};

/// When a file changed on both sides since the last sync.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BothChanged {
    /// Keep both: OneDrive's copy keeps the name, and this computer's copy is
    /// renamed `name (conflict from HOST).ext` and synced too.
    KeepBoth,
    /// The copy modified last wins; the other is replaced.
    NewestWins,
}

/// When one side deleted a file the other side changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeleteVsEdit {
    /// The edit survives and comes back on the side that deleted it.
    KeepEdit,
    /// The deletion wins.
    Delete,
}

/// A rule for file names matching `pattern` (`*` and `?` wildcards, matched
/// against the file name, not the folder).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub pattern: String,
    pub both_changed: BothChanged,
    pub delete_vs_edit: DeleteVsEdit,
}

impl Rule {
    pub fn new(pattern: &str, both_changed: BothChanged, delete_vs_edit: DeleteVsEdit) -> Self {
        Self {
            pattern: pattern.to_string(),
            both_changed,
            delete_vs_edit,
        }
    }
}

/// The rules for a pair, tried in order; the first match decides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rules {
    pub rules: Vec<Rule>,
    /// For names no rule matches.
    pub fallback: Rule,
    /// Names never synced in either direction.
    pub ignore: Vec<String>,
}

impl Default for Rules {
    /// Waypoint's rules: trips keep both, locks and pages go to the newest,
    /// and a released lock (a deleted `.trip.lock`) beats a stale edit.
    fn default() -> Self {
        Self {
            rules: vec![
                Rule::new("*.trip.json", BothChanged::KeepBoth, DeleteVsEdit::KeepEdit),
                Rule::new("*.trip.lock", BothChanged::NewestWins, DeleteVsEdit::Delete),
                Rule::new("*.html", BothChanged::NewestWins, DeleteVsEdit::KeepEdit),
            ],
            fallback: Rule::new("*", BothChanged::KeepBoth, DeleteVsEdit::KeepEdit),
            ignore: [
                "*.odsync-tmp",
                ".DS_Store",
                "Thumbs.db",
                "desktop.ini",
                "~$*",
                ".~lock.*",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
        }
    }
}

impl Rules {
    /// The rule for the file at `path` (relative, `/`-separated).
    pub fn for_path(&self, path: &str) -> &Rule {
        let name = file_name(path);
        self.rules
            .iter()
            .find(|r| glob_match(&r.pattern, name))
            .unwrap_or(&self.fallback)
    }

    pub fn is_ignored(&self, path: &str) -> bool {
        let name = file_name(path);
        self.ignore.iter().any(|p| glob_match(p, name))
    }
}

pub(crate) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `*` matches any run of characters, `?` any one; case-insensitive, as
/// OneDrive is.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// `name (conflict from HOST).ext`, where `.ext` is everything from the first
/// dot after the start of the name (so `a.trip.json` keeps `.trip.json`).
/// `n` above 1 adds a number, for when that name is taken too.
pub fn conflict_name(path: &str, host: &str, n: u32) -> String {
    let (dir, name) = match path.rfind('/') {
        Some(i) => (&path[..=i], &path[i + 1..]),
        None => ("", path),
    };
    let split = name
        .char_indices()
        .skip(1)
        .find(|&(_, c)| c == '.')
        .map(|(i, _)| i)
        .unwrap_or(name.len());
    let (stem, ext) = name.split_at(split);
    let number = if n > 1 {
        format!(" {n}")
    } else {
        String::new()
    };
    format!("{dir}{stem} (conflict from {host}{number}){ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("*.trip.json", "Lakes.trip.json"));
        assert!(glob_match("*.TRIP.json", "lakes.trip.json"));
        assert!(!glob_match("*.trip.json", "lakes.trip.json.bak"));
        assert!(glob_match("~$*", "~$doc.docx"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn rule_choice() {
        let r = Rules::default();
        assert_eq!(
            r.for_path("x/lakes.trip.lock").both_changed,
            BothChanged::NewestWins
        );
        assert_eq!(
            r.for_path("x/lakes.trip.json").both_changed,
            BothChanged::KeepBoth
        );
        assert_eq!(r.for_path("notes.txt").pattern, "*");
        assert!(r.is_ignored("a/b/.DS_Store"));
        assert!(r.is_ignored("x.json.odsync-tmp"));
    }

    #[test]
    fn conflict_names() {
        assert_eq!(
            conflict_name("trips/lakes.trip.json", "ATLAS", 1),
            "trips/lakes (conflict from ATLAS).trip.json"
        );
        assert_eq!(
            conflict_name("readme", "pc", 2),
            "readme (conflict from pc 2)"
        );
        assert_eq!(
            conflict_name(".hidden.txt", "pc", 1),
            ".hidden (conflict from pc).txt"
        );
    }
}
