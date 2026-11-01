//! Attaching a terminal to the rdreh server: set the terminal up, forward
//! input events (JSON lines), and write the server's frames to stdout.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, EventStream,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::{cursor::SetCursorStyle, execute, terminal};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::ipc::Request;
use crate::server::{MARK_DETACHED, MARK_EXITED};

pub enum End {
    Detached,
    Exited,
    Lost,
}

struct TermGuard {
    enhanced: bool,
}

impl TermGuard {
    fn enter() -> Result<TermGuard> {
        terminal::enable_raw_mode().context("attaching needs a terminal")?;
        let mut out = std::io::stdout();
        execute!(out, terminal::EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
        // Kitty protocol: Esc and Alt+key arrive unambiguously, with no escape delay.
        let enhanced = terminal::supports_keyboard_enhancement().unwrap_or(false);
        if enhanced {
            execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
        }
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(enhanced);
            hook(info);
        }));
        Ok(TermGuard { enhanced })
    }
}

fn restore(enhanced: bool) {
    let mut out = std::io::stdout();
    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        SetCursorStyle::DefaultUserShape,
        DisableBracketedPaste,
        DisableMouseCapture,
        terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    );
    let _ = terminal::disable_raw_mode();
    let _ = out.flush();
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore(self.enhanced);
    }
}

pub async fn attach(socket: &Path) -> Result<End> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let (mut rd, mut wr) = stream.into_split();
    let guard = TermGuard::enter()?;
    let (cols, rows) = terminal::size()?;
    let mut hello = serde_json::to_vec(&Request::Attach { cols, rows })?;
    hello.push(b'\n');
    wr.write_all(&hello).await?;

    let mut input = EventStream::new();
    let mut stdout = tokio::io::stdout();
    let mut buf = vec![0u8; 64 * 1024];
    let mut tail: Vec<u8> = Vec::new();
    let mut first = true;
    loop {
        tokio::select! {
            ev = input.next() => match ev {
                Some(Ok(ev)) => {
                    let mut line = serde_json::to_vec(&ev)?;
                    line.push(b'\n');
                    if wr.write_all(&line).await.is_err() {
                        break;
                    }
                }
                _ => break,
            },
            n = rd.read(&mut buf) => match n {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // A pre-server rdreh answers the attach with a JSON error.
                    if first && buf[..n].starts_with(b"{\"ok\"") {
                        drop(guard);
                        anyhow::bail!(
                            "the running rdreh is an older version that cannot be attached to. \
                             Quit it in its window (Ctrl+b q), then run `rdr` again."
                        );
                    }
                    first = false;
                    stdout.write_all(&buf[..n]).await?;
                    stdout.flush().await?;
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 64 {
                        tail.drain(..tail.len() - 64);
                    }
                }
            },
        }
    }
    drop(guard);
    let ends = |m: &[u8]| tail.windows(m.len()).any(|w| w == m);
    Ok(if ends(MARK_EXITED) {
        End::Exited
    } else if ends(MARK_DETACHED) {
        End::Detached
    } else {
        End::Lost
    })
}
