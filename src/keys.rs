//! Translate crossterm input events back into the byte sequences a program
//! running in a PTY expects (legacy xterm encoding).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

fn modifier_param(m: KeyModifiers) -> u8 {
    1 + u8::from(m.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(m.contains(KeyModifiers::ALT))
        + 4 * u8::from(m.contains(KeyModifiers::CONTROL))
}

fn ctrl_char(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' => c as u8 - b'a' + 1,
        'A'..='Z' => c as u8 - b'A' + 1,
        '@' | ' ' | '2' => 0,
        '[' | '3' => 27,
        '\\' | '4' => 28,
        ']' | '5' => 29,
        '^' | '6' => 30,
        '_' | '7' | '/' => 31,
        '8' | '?' => 127,
        _ => return None,
    })
}

/// Cursor-style keys: `ESC [ X` (or `ESC O X` in application mode), and
/// `ESC [ 1 ; m X` when modified.
fn cursor_key(letter: u8, mods: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    let m = modifier_param(mods);
    if m == 1 {
        vec![0x1b, if app_cursor { b'O' } else { b'[' }, letter]
    } else {
        format!("\x1b[1;{m}{}", letter as char).into_bytes()
    }
}

/// Tilde keys: `ESC [ n ~` / `ESC [ n ; m ~`.
fn tilde_key(n: u8, mods: KeyModifiers) -> Vec<u8> {
    let m = modifier_param(mods);
    if m == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{m}~").into_bytes()
    }
}

pub fn encode_key(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let with_alt = |mut v: Vec<u8>| {
        if alt {
            v.insert(0, 0x1b);
        }
        v
    };
    Some(match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                with_alt(vec![ctrl_char(c)?])
            } else {
                let mut buf = [0u8; 4];
                with_alt(c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
        }
        // Shift+Enter -> Meta+Enter: the "insert newline" chord agent TUIs accept.
        KeyCode::Enter if mods.contains(KeyModifiers::SHIFT) => vec![0x1b, b'\r'],
        KeyCode::Enter => with_alt(vec![b'\r']),
        KeyCode::Tab => with_alt(vec![b'\t']),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace if ctrl => with_alt(vec![0x08]),
        KeyCode::Backspace => with_alt(vec![0x7f]),
        KeyCode::Esc => with_alt(vec![0x1b]),
        KeyCode::Up => cursor_key(b'A', mods, app_cursor),
        KeyCode::Down => cursor_key(b'B', mods, app_cursor),
        KeyCode::Right => cursor_key(b'C', mods, app_cursor),
        KeyCode::Left => cursor_key(b'D', mods, app_cursor),
        KeyCode::Home => cursor_key(b'H', mods, app_cursor),
        KeyCode::End => cursor_key(b'F', mods, app_cursor),
        KeyCode::Insert => tilde_key(2, mods),
        KeyCode::Delete => tilde_key(3, mods),
        KeyCode::PageUp => tilde_key(5, mods),
        KeyCode::PageDown => tilde_key(6, mods),
        KeyCode::F(n @ 1..=4) => {
            let letter = b"PQRS"[usize::from(n - 1)];
            let m = modifier_param(mods);
            if m == 1 {
                vec![0x1b, b'O', letter]
            } else {
                format!("\x1b[1;{m}{}", letter as char).into_bytes()
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)];
            tilde_key(code, mods)
        }
        _ => return None,
    })
}

/// Encode a mouse event for a child that enabled mouse reporting. `col`/`row`
/// are 0-based and relative to the pane's inner area.
pub fn encode_mouse(
    ev: &MouseEvent,
    col: u16,
    row: u16,
    mode: MouseProtocolMode,
    enc: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    let button_code = |b: MouseButton| match b {
        MouseButton::Left => 0u8,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut code, release) = match ev.kind {
        MouseEventKind::Down(b) => (button_code(b), false),
        MouseEventKind::Up(b) => {
            if mode == MouseProtocolMode::Press {
                return None;
            }
            (button_code(b), true)
        }
        MouseEventKind::Drag(b) => {
            if !matches!(mode, MouseProtocolMode::ButtonMotion | MouseProtocolMode::AnyMotion) {
                return None;
            }
            (button_code(b) + 32, false)
        }
        MouseEventKind::Moved => {
            if mode != MouseProtocolMode::AnyMotion {
                return None;
            }
            (3 + 32, false)
        }
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    match enc {
        MouseProtocolEncoding::Sgr => Some(
            format!("\x1b[<{code};{x};{y}{}", if release { 'm' } else { 'M' }).into_bytes(),
        ),
        _ => {
            // X10/normal encoding: values offset by 32, capped at 223.
            if x > 223 || y > 223 {
                return None;
            }
            let c = if release { 3 } else { code };
            Some(vec![0x1b, b'[', b'M', 32 + c, 32 + x as u8, 32 + y as u8])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, mods: KeyModifiers) -> Vec<u8> {
        encode_key(KeyEvent::new(code, mods), false).unwrap()
    }

    #[test]
    fn plain_and_control_chars() {
        assert_eq!(k(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(k(KeyCode::Char('é'), KeyModifiers::NONE), "é".as_bytes());
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CONTROL), [3]);
        assert_eq!(k(KeyCode::Char('b'), KeyModifiers::CONTROL), [2]);
        assert_eq!(k(KeyCode::Char('x'), KeyModifiers::ALT), b"\x1bx");
        assert_eq!(k(KeyCode::Char('d'), KeyModifiers::CONTROL | KeyModifiers::ALT), [0x1b, 4]);
    }

    #[test]
    fn special_keys() {
        assert_eq!(k(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(k(KeyCode::Enter, KeyModifiers::SHIFT), b"\x1b\r");
        assert_eq!(k(KeyCode::Backspace, KeyModifiers::NONE), [0x7f]);
        assert_eq!(k(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
        assert_eq!(k(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(k(KeyCode::PageUp, KeyModifiers::CONTROL), b"\x1b[5;5~");
        assert_eq!(k(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(k(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
        assert_eq!(k(KeyCode::F(12), KeyModifiers::SHIFT), b"\x1b[24;2~");
    }

    #[test]
    fn arrows_respect_application_mode_and_modifiers() {
        assert_eq!(k(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        let app = encode_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), true).unwrap();
        assert_eq!(app, b"\x1bOA");
        assert_eq!(k(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(k(KeyCode::Right, KeyModifiers::ALT), b"\x1b[1;3C");
    }

    #[test]
    fn sgr_mouse() {
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let out = encode_mouse(&ev, 4, 9, MouseProtocolMode::PressRelease, MouseProtocolEncoding::Sgr);
        assert_eq!(out.unwrap(), b"\x1b[<0;5;10M");
        let up = MouseEvent { kind: MouseEventKind::Up(MouseButton::Left), ..ev };
        assert!(encode_mouse(&up, 0, 0, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr).is_none());
        let wheel = MouseEvent { kind: MouseEventKind::ScrollDown, ..ev };
        assert_eq!(
            encode_mouse(&wheel, 0, 0, MouseProtocolMode::Press, MouseProtocolEncoding::Sgr).unwrap(),
            b"\x1b[<65;1;1M"
        );
    }
}
