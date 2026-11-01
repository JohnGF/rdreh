//! Pasted file lists. Copying files in a file manager and pasting them into a
//! terminal gives one path (or `file://` URI) per line. Claude Code copes with
//! that, but most agents (OpenCode, shells) see several lines of text. When a
//! paste is nothing but files, rewrite it the way terminals do for a drop:
//! shell-escaped paths separated by spaces, with a trailing space.

use std::path::Path;

/// The paste rewritten as a space-separated path list, or `None` to leave it
/// alone. Plain absolute paths are only accepted if they exist, so ordinary
/// text that happens to start with `/` passes through untouched.
pub fn file_list(text: &str) -> Option<String> {
    // Terminals often turn newlines into CR on paste (Alacritty, tmux).
    let lines: Vec<&str> = text.split(['\r', '\n']).map(str::trim).filter(|l| !l.is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    let mut any_uri = false;
    let mut paths = Vec::with_capacity(lines.len());
    for line in &lines {
        if let Some(p) = from_uri(line) {
            any_uri = true;
            paths.push(p);
        } else if line.starts_with('/') && Path::new(line).exists() {
            paths.push((*line).to_string());
        } else {
            return None;
        }
    }
    // A single plain path is already fine as it is.
    if paths.len() == 1 && !any_uri {
        return None;
    }
    let mut out = paths.iter().map(|p| escape(p)).collect::<Vec<_>>().join(" ");
    out.push(' ');
    Some(out)
}

/// `file:///a%20b` or `file://host/a%20b` to `/a b`.
fn from_uri(s: &str) -> Option<String> {
    let rest = s.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let bytes = percent_decode(path)?;
    String::from_utf8(bytes).ok()
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Backslash-escape shell metacharacters, as kitty and Ghostty do for drops.
fn escape(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for c in p.chars() {
        if c.is_whitespace() || "\\'\"`$&|;<>()[]{}*?!#~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uris_become_escaped_paths() {
        let text = "file:///home/j/My%20Shot.png\nfile:///tmp/a(1).txt\n";
        assert_eq!(file_list(text).unwrap(), "/home/j/My\\ Shot.png /tmp/a\\(1\\).txt ");
    }

    #[test]
    fn uri_with_host() {
        assert_eq!(file_list("file://laptop/etc/hosts").unwrap(), "/etc/hosts ");
    }

    #[test]
    fn cr_separated() {
        let text = "file:///a.png\r\rfile:///b.png\r\r";
        assert_eq!(file_list(text).unwrap(), "/a.png /b.png ");
    }

    #[test]
    fn existing_paths_one_per_line() {
        let dir = std::env::temp_dir();
        let a = dir.join("rdr paste a");
        let b = dir.join("rdr-paste-b");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        let text = format!("{}\r\n{}\r\n", a.display(), b.display());
        let want = format!("{} {} ", escape(&a.to_string_lossy()), b.display());
        assert_eq!(file_list(&text).unwrap(), want);
        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
    }

    #[test]
    fn leaves_other_text_alone() {
        assert_eq!(file_list("hello\nworld"), None);
        assert_eq!(file_list("/no/such/path/x\n/no/such/path/y"), None);
        assert_eq!(file_list("/etc/hosts"), None);
        assert_eq!(file_list("/etc/hosts\nsome text"), None);
        assert_eq!(file_list(""), None);
        assert_eq!(file_list("file:///bad%zz"), None);
    }
}
