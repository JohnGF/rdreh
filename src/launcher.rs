//! Opening rdr in a new terminal window, for launches without a TTY (KRunner,
//! app menu) and for `--term`. Picking the terminal and building its argv are
//! pure functions so they can be unit-tested without opening windows.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Tried in order when neither `--term`, `general.terminal` nor `$TERMINAL` is set.
pub const KNOWN: &[&str] = &[
    "ghostty",
    "kitty",
    "wezterm",
    "foot",
    "alacritty",
    "konsole",
    "gnome-terminal",
    "xdg-terminal-exec",
    "xterm",
];

pub fn find_in_path(cmd: &str) -> Option<PathBuf> {
    if cmd.contains('/') {
        return Path::new(cmd).is_file().then(|| PathBuf::from(cmd));
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).map(|d| d.join(cmd)).find(|p| p.is_file())
}

pub fn on_path(cmd: &str) -> bool {
    find_in_path(cmd).is_some()
}

/// KDE Plasma's task manager (Wayland) identifies a window by the file name
/// of its executable, not by its app id: every Alacritty window is grouped
/// under Alacritty no matter what `--class` says. Running the terminal from a
/// copy of its binary named `rdr` gives rdr its own taskbar entry, icon and
/// badge. The copy lives in `<data_dir>/rdr/identity/rdr` (a free reflink on
/// btrfs/xfs) and is refreshed when the terminal binary changes. Scripts are
/// skipped: their process is the interpreter, so a copy would not help.
pub fn identity_binary(terminal: &Path, data_dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let src = std::fs::canonicalize(terminal)?;
    let mut magic = [0u8; 4];
    if std::fs::File::open(&src)?.read_exact(&mut magic).is_err() || &magic != b"\x7fELF" {
        return Ok(None);
    }
    let meta = std::fs::metadata(&src)?;
    // Upgrades replace the file (new inode); edits change size or mtime.
    use std::os::unix::fs::MetadataExt;
    let stamp_text = format!(
        "{}\n{} {} {} {}.{}\n",
        src.display(),
        meta.dev(),
        meta.ino(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec()
    );

    let dir = data_dir.join("rdr/identity");
    let dst = dir.join("rdr");
    let stamp = dir.join("source");
    if dst.is_file() && std::fs::read_to_string(&stamp).is_ok_and(|s| s == stamp_text) {
        return Ok(Some(dst));
    }
    std::fs::create_dir_all(&dir)?;
    // Copy then rename, so a running copy is never overwritten in place.
    let tmp = dir.join(format!(".rdr.{}", std::process::id()));
    std::fs::copy(&src, &tmp)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, &dst)?;
    std::fs::write(&stamp, stamp_text)?;
    Ok(Some(dst))
}

/// Base command (program plus user-supplied flags) of the terminal to use.
/// Explicit choices (flag, config) are trusted as given; the environment and
/// the built-in list are filtered by what is installed.
pub fn resolve(
    flag: Option<&str>,
    configured: Option<&str>,
    env_terminal: Option<&str>,
    installed: impl Fn(&str) -> bool,
) -> Option<Vec<String>> {
    let split = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
    if let Some(t) = flag.or(configured).filter(|t| !t.trim().is_empty()) {
        return Some(split(t));
    }
    if let Some(t) = env_terminal.map(split).filter(|v| v.first().is_some_and(|p| installed(p))) {
        return Some(t);
    }
    KNOWN.iter().find(|t| installed(t)).map(|t| vec![t.to_string()])
}

/// Full argv: terminal, its flags for class/title/maximised and "run this",
/// then `inner` (our own exe and arguments).
pub fn argv(base: &[String], inner: &[String]) -> Vec<String> {
    let name = base
        .first()
        .and_then(|p| Path::new(p).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let extra: &[&str] = match name.as_str() {
        // WM class "rdr" matches StartupWMClass in rdr.desktop: own taskbar icon + badge.
        "alacritty" => &["--class", "rdr,rdr", "--title", "rdreh", "-o", "window.startup_mode=\"Maximized\"", "-e"],
        "kitty" => &["--class", "rdr", "--title", "rdreh", "--start-as=maximized"],
        "wezterm" => &["start", "--class", "rdr", "--"],
        "foot" => &["--app-id=rdr", "--title=rdreh", "--maximized"],
        "ghostty" => &["--title=rdreh", "-e"],
        "konsole" => &["--separate", "-e"],
        "gnome-terminal" => &["--maximize", "--title=rdreh", "--"],
        "xdg-terminal-exec" => &[],
        _ => &["-e"],
    };
    base.iter().cloned().chain(extra.iter().map(|s| s.to_string())).chain(inner.iter().cloned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_copy_for_binaries_only_and_refreshes() {
        let dir = tempfile::tempdir().unwrap();
        let src_dir = dir.path().join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        let term = src_dir.join("term");
        std::fs::copy("/bin/true", &term).unwrap();

        let copy = identity_binary(&term, dir.path()).unwrap().expect("ELF gets a copy");
        assert_eq!(copy.file_name().unwrap(), "rdr");
        assert!(std::fs::read(&copy).unwrap() == std::fs::read(&term).unwrap(), "copy differs");
        assert!(std::process::Command::new(&copy).status().unwrap().success(), "copy must run");

        // Unchanged source: reused. Changed source: refreshed.
        assert_eq!(identity_binary(&term, dir.path()).unwrap(), Some(copy.clone()));
        std::fs::remove_file(&term).unwrap(); // like a package upgrade: new file
        std::fs::copy("/bin/false", &term).unwrap();
        identity_binary(&term, dir.path()).unwrap();
        assert!(std::fs::read(&copy).unwrap() == std::fs::read("/bin/false").unwrap(), "copy not refreshed");

        // Scripts are left alone.
        let script = src_dir.join("script-term");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        assert_eq!(identity_binary(&script, &dir.path().join("other")).unwrap(), None);
    }

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn explicit_choice_wins_and_keeps_flags() {
        let none = |_: &str| false;
        assert_eq!(resolve(Some("foot"), Some("kitty"), Some("xterm"), none), Some(s(&["foot"])));
        assert_eq!(resolve(None, Some("kitty -1"), None, none), Some(s(&["kitty", "-1"])));
    }

    #[test]
    fn env_then_builtin_list_filtered_by_installed() {
        let only = |names: &'static [&'static str]| move |p: &str| names.contains(&p);
        assert_eq!(resolve(None, None, Some("wezterm"), only(&["wezterm", "kitty"])), Some(s(&["wezterm"])));
        // $TERMINAL not installed: fall through to the list order.
        assert_eq!(resolve(None, None, Some("nope"), only(&["konsole", "kitty"])), Some(s(&["kitty"])));
        assert_eq!(resolve(None, Some("  "), None, only(&["alacritty"])), Some(s(&["alacritty"])));
        assert_eq!(resolve(None, None, None, only(&[])), None);
    }

    #[test]
    fn argv_per_terminal() {
        let inner = s(&["/bin/rdr", "--terminal"]);
        assert_eq!(
            argv(&s(&["alacritty"]), &inner),
            s(&["alacritty", "--class", "rdr,rdr", "--title", "rdreh", "-o", "window.startup_mode=\"Maximized\"", "-e", "/bin/rdr", "--terminal"])
        );
        assert_eq!(argv(&s(&["/usr/bin/wezterm"]), &inner)[1..5], s(&["start", "--class", "rdr", "--"]));
        assert_eq!(argv(&s(&["kitty", "-1"]), &inner)[..3], s(&["kitty", "-1", "--class"]));
        assert_eq!(argv(&s(&["xdg-terminal-exec"]), &inner), s(&["xdg-terminal-exec", "/bin/rdr", "--terminal"]));
        assert_eq!(argv(&s(&["st"]), &inner), s(&["st", "-e", "/bin/rdr", "--terminal"]));
    }
}
