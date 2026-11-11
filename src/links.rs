//! Ctrl+click on a URL or file path inside a pane opens it with the desktop
//! opener. Relative paths resolve against the pane's own working directory,
//! which the outer terminal cannot know.

use std::path::{Path, PathBuf};

/// What a clicked token resolved to.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    Url(String),
    File(PathBuf),
}

/// Characters that never belong to a link or path token.
fn is_delim(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>' | '|' | '│' | '(' | ')' | '[' | ']' | '{' | '}')
}

/// The visible text of `row`, joined with neighbouring soft-wrapped rows,
/// plus the char index of (`row`, `col`) in it.
pub fn line_at(screen: &vt100::Screen, row: u16, col: u16) -> Option<(Vec<char>, usize)> {
    let (_, cols) = screen.size();
    let mut start = row;
    while start > 0 && screen.row_wrapped(start - 1) {
        start -= 1;
    }
    let mut end = row;
    while screen.row_wrapped(end) && screen.cell(end + 1, 0).is_some() {
        end += 1;
    }
    let mut chars = Vec::new();
    let mut hit = None;
    for r in start..=end {
        for c in 0..cols.get() {
            let cell = screen.cell(r, c)?;
            if cell.is_wide_continuation() {
                if r == row && c == col {
                    hit = Some(chars.len().saturating_sub(1));
                }
                continue;
            }
            if r == row && c == col {
                hit = Some(chars.len());
            }
            let s = cell.contents();
            if s.is_empty() {
                chars.push(' ');
            } else {
                chars.extend(s.chars());
            }
        }
    }
    Some((chars, hit?))
}

/// The link-ish token covering `idx`, with surrounding punctuation and a
/// trailing `:line[:col]` stripped.
pub fn token_at(line: &[char], idx: usize) -> Option<String> {
    if idx >= line.len() || is_delim(line[idx]) {
        return None;
    }
    let mut s = idx;
    while s > 0 && !is_delim(line[s - 1]) {
        s -= 1;
    }
    let mut e = idx + 1;
    while e < line.len() && !is_delim(line[e]) {
        e += 1;
    }
    let mut tok: String = line[s..e].iter().collect();
    // Markdown bullets and prose punctuation around the word.
    let tok_trimmed = tok.trim_start_matches(['*', '•', ',', ';']);
    tok = tok_trimmed.trim_end_matches(['.', ',', ':', ';', '!', '?', '*']).to_string();
    if !tok.contains("://") {
        tok = strip_line_suffix(&tok).to_string();
    }
    (!tok.is_empty()).then_some(tok)
}

/// `src/app.rs:120:5` -> `src/app.rs`.
fn strip_line_suffix(tok: &str) -> &str {
    let mut t = tok;
    for _ in 0..2 {
        match t.rsplit_once(':') {
            Some((head, num)) if !head.is_empty() && !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) => {
                t = head;
            }
            _ => break,
        }
    }
    t
}

/// Turns a token into something openable: a web URL, or an existing file
/// or directory (relative paths are taken from `cwd`).
pub fn resolve(tok: &str, cwd: &Path) -> Option<Target> {
    if tok.starts_with("http://") || tok.starts_with("https://") {
        return Some(Target::Url(tok.to_string()));
    }
    let path = if let Some(p) = tok.strip_prefix("file://") {
        PathBuf::from(p)
    } else if let Some(rest) = tok.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        let p = PathBuf::from(tok);
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    path.exists().then_some(Target::File(path))
}

/// Hands the target to the desktop opener without blocking the UI.
pub fn open(target: &Target) -> std::io::Result<()> {
    let arg = match target {
        Target::Url(u) => u.as_str().into(),
        Target::File(p) => p.as_os_str().to_owned(),
    };
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut child = std::process::Command::new(opener)
        .arg(arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(s: &str, at: &str) -> Option<String> {
        let line: Vec<char> = s.chars().collect();
        let byte = s.find(at).unwrap();
        token_at(&line, s[..byte].chars().count())
    }

    #[test]
    fn token_strips_prose_punctuation() {
        let s = "• Summary Metrics JSON: sub-01_ses-01_fnirs_report.json.";
        assert_eq!(tok(s, "fnirs").as_deref(), Some("sub-01_ses-01_fnirs_report.json"));
        let s = "Figure: sub-01_analysis.png (includes SCI bars,";
        assert_eq!(tok(s, "analysis").as_deref(), Some("sub-01_analysis.png"));
        assert_eq!(tok("see `src/app.rs:120:5` here", "app").as_deref(), Some("src/app.rs"));
        assert_eq!(tok("**docs/README.md**", "README").as_deref(), Some("docs/README.md"));
        assert_eq!(tok("a  b", " b").as_deref(), None);
    }

    #[test]
    fn token_keeps_urls_whole() {
        let s = "go to <https://example.com/a:b?x=1>.";
        assert_eq!(tok(s, "example").as_deref(), Some("https://example.com/a:b?x=1"));
        assert_eq!(tok("https://x.org/p.", "x.org").as_deref(), Some("https://x.org/p"));
    }

    #[test]
    fn resolve_against_cwd() {
        let dir = std::env::temp_dir().join(format!("rdr-links-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("fig.png"), b"").unwrap();
        assert_eq!(resolve("fig.png", &dir), Some(Target::File(dir.join("fig.png"))));
        assert_eq!(resolve("missing.png", &dir), None);
        assert_eq!(resolve("https://a.b/c", &dir), Some(Target::Url("https://a.b/c".into())));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn line_joins_soft_wrapped_rows() {
        let mut p = vt100::Parser::new(3.try_into().unwrap(), 10.try_into().unwrap(), 0);
        p.process(b"x abcdefghijkl.png y");
        let s = p.screen();
        let (line, idx) = line_at(s, 1, 2).unwrap();
        assert_eq!(token_at(&line, idx).as_deref(), Some("abcdefghijkl.png"));
    }
}
