//! `rdr install` / `rdr uninstall`: put the embedded desktop entry and icon in
//! the user's XDG data dir, so KRunner, the app menu and the taskbar show rdr
//! with its own icon. No files outside the binary are needed.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub const ICON_SVG: &str = include_str!("../packaging/rdr.svg");
const DESKTOP_TEMPLATE: &str = include_str!("../packaging/rdr.desktop");

fn data_home() -> Result<PathBuf> {
    dirs::data_dir().context("cannot determine XDG data dir")
}

fn targets() -> Result<(PathBuf, PathBuf)> {
    let data = data_home()?;
    Ok((
        data.join("applications/rdr.desktop"),
        data.join("icons/hicolor/scalable/apps/rdr.svg"),
    ))
}

/// Desktop entry pointing at `exe` (quoted if it contains spaces).
pub fn desktop_entry(exe: &Path) -> String {
    let exe = exe.display().to_string();
    let quoted = if exe.contains(' ') { format!("\"{exe}\"") } else { exe.clone() };
    DESKTOP_TEMPLATE
        .lines()
        .map(|l| {
            if l.starts_with("Exec=") {
                format!("Exec={quoted}")
            } else if l.starts_with("TryExec=") {
                format!("TryExec={exe}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// Write `contents` to `path`, replacing a symlink instead of writing through
/// it (the target might be a file in someone's dotfiles).
fn write_fresh(path: &Path, contents: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if path.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        std::fs::remove_file(path)?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn refresh_caches() {
    let run = |cmd: &str, args: &[&str]| {
        let _ = std::process::Command::new(cmd)
            .args(args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    };
    if let Ok(data) = data_home() {
        let apps = data.join("applications").display().to_string();
        let icons = data.join("icons/hicolor").display().to_string();
        run("update-desktop-database", &[&apps]);
        run("gtk-update-icon-cache", &["-q", "-t", "-f", &icons]);
    }
    run("kbuildsycoca6", &[]); // KRunner / Plasma menu
}

pub fn install(refresh: bool) -> Result<Vec<PathBuf>> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let (desktop, icon) = targets()?;
    write_fresh(&icon, ICON_SVG)?;
    write_fresh(&desktop, &desktop_entry(&exe))?;
    if refresh {
        refresh_caches();
    }
    Ok(vec![desktop, icon])
}

pub fn uninstall(refresh: bool) -> Result<Vec<PathBuf>> {
    let (desktop, icon) = targets()?;
    let mut removed = Vec::new();
    for p in [desktop, icon] {
        if p.symlink_metadata().is_ok() {
            std::fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
            removed.push(p);
        }
    }
    if refresh {
        refresh_caches();
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_entry_points_at_binary() {
        let d = desktop_entry(Path::new("/opt/my tools/rdr"));
        assert!(d.contains("\nExec=\"/opt/my tools/rdr\"\n"));
        assert!(d.contains("\nTryExec=/opt/my tools/rdr\n"));
        assert!(d.contains("\nIcon=rdr\n"));
        assert!(d.contains("\nStartupWMClass=rdr\n"));
    }

    #[test]
    fn icon_is_svg() {
        assert!(ICON_SVG.trim_start().starts_with("<svg"));
    }
}
