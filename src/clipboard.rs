//! System and terminal clipboard integration (OSC 52 + wl-copy / xclip).

/// Encodes binary data as standard base64 (RFC 4648).
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Copies text to the system clipboard via external tools if present, and
/// returns an OSC 52 terminal escape sequence to set the terminal clipboard.
pub fn copy_to_clipboard(text: &str) -> Vec<u8> {
    let text_clone = text.to_string();
    std::thread::spawn(move || {
        copy_via_external(&text_clone);
    });

    let encoded = base64_encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07").into_bytes()
}

fn copy_via_external(text: &str) {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && spawn_pipe_input("wl-copy", &[], text).is_ok() {
        return;
    }
    if std::env::var_os("DISPLAY").is_some() {
        if spawn_pipe_input("xclip", &["-selection", "clipboard"], text).is_ok() {
            return;
        }
        if spawn_pipe_input("xsel", &["--clipboard", "--input"], text).is_ok() {
            return;
        }
    }
    if cfg!(target_os = "macos") {
        let _ = spawn_pipe_input("pbcopy", &[], text);
    }
}

/// The system clipboard for Ctrl+b p. A file list (`text/uri-list`, what file
/// managers copy) wins over plain text. Terminals like Alacritty only read
/// plain text, so pasting copied files through them gives nothing.
pub fn read_clipboard() -> Option<String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let x11 = std::env::var_os("DISPLAY").is_some();
    let mut tries: Vec<(&str, Vec<&str>)> = Vec::new();
    if wayland {
        tries.push(("wl-paste", vec!["--no-newline", "--type", "text/uri-list"]));
        tries.push(("wl-paste", vec!["--no-newline"]));
    }
    if x11 {
        tries.push(("xclip", vec!["-selection", "clipboard", "-o", "-t", "text/uri-list"]));
        tries.push(("xclip", vec!["-selection", "clipboard", "-o"]));
    }
    if cfg!(target_os = "macos") {
        tries.push(("pbpaste", vec![]));
    }
    tries.into_iter().find_map(|(cmd, args)| run_capture(cmd, &args)).filter(|s| !s.is_empty())
}

/// stdout of `cmd` if it exits successfully within a second. A clipboard
/// owner that never answers must not freeze the UI.
fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    use std::io::Read;
    use std::time::Duration;
    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let Ok(buf) = rx.recv_timeout(Duration::from_secs(1)) else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    child.wait().ok()?.success().then(|| String::from_utf8_lossy(&buf).into_owned())
}

fn spawn_pipe_input(cmd: &str, args: &[&str], input: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let _ = child.wait();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"hello world\n"), "aGVsbG8gd29ybGQK");
    }

    #[test]
    fn test_osc52_format() {
        let osc = copy_to_clipboard("hi");
        assert_eq!(osc, b"\x1b]52;c;aGk=\x07");
    }
}
