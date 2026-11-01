//! Tiny /proc readers (Linux).

use std::path::PathBuf;

pub fn comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

pub fn ppid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised comm: state ppid ...
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// File name of the program a process is running (`/proc/<pid>/exe`).
pub fn exe_name(pid: u32) -> Option<String> {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
    Some(exe.file_name()?.to_string_lossy().into_owned())
}

pub fn cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// `pid` and its ancestors up to (excluding) init.
pub fn ancestry(pid: u32) -> impl Iterator<Item = u32> {
    let mut cur = Some(pid);
    std::iter::from_fn(move || {
        let p = cur.filter(|p| *p > 1)?;
        cur = ppid(p);
        Some(p)
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn own_process() {
        let me = std::process::id();
        assert!(super::comm(me).is_some());
        assert!(super::ancestry(me).count() >= 1);
        assert_eq!(super::ancestry(me).next(), Some(me));
    }
}
