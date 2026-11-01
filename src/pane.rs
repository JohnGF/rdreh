//! One pane = one child process on a PTY + a vt100 screen that mirrors it.
//! A blocking reader thread per pane feeds the parser even when the pane is
//! hidden, so background agents always have accurate screen state.

use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc::UnboundedSender;

use crate::app::Event;
use crate::procinfo;

pub type PaneId = u32;

/// Answers the terminal queries vt100 leaves unhandled. Without these, shells
/// such as fish 4 stall at startup waiting for a Device Attributes reply.
#[derive(Default)]
pub struct TermCallbacks {
    pub responses: Vec<u8>,
    pub title: Option<String>,
}

fn first_param(params: &[&[u16]]) -> u16 {
    params.first().and_then(|p| p.first()).copied().unwrap_or(0)
}

impl vt100::Callbacks for TermCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).trim().to_string());
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let (row, col) = screen.cursor_position();
        let reply = match (i1, c, first_param(params)) {
            (None, 'c', 0) => "\x1b[?62;22c".to_string(),
            (Some(b'>'), 'c', 0) => "\x1b[>1;10;0c".to_string(),
            (None, 'n', 5) => "\x1b[0n".to_string(),
            (None, 'n', 6) => format!("\x1b[{};{}R", row + 1, col + 1),
            (Some(b'?'), 'n', 6) => format!("\x1b[?{};{}R", row + 1, col + 1),
            _ => return,
        };
        self.responses.extend_from_slice(reply.as_bytes());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        // OSC 10/11 colour queries: report a dark theme so TUIs pick dark palettes.
        if params.len() == 2 && params[1] == b"?" {
            let reply = match params[0] {
                b"10" => "\x1b]10;rgb:d8d8/dede/e9e9\x1b\\",
                b"11" => "\x1b]11;rgb:2e2e/3434/4040\x1b\\",
                _ => return,
            };
            self.responses.extend_from_slice(reply.as_bytes());
        }
    }
}

pub type Parser = vt100::Parser<TermCallbacks>;

/// Interactive shells whose line editor clears the screen on Ctrl+L.
const CLEARS_ON_CTRL_L: &[&str] = &["bash", "zsh", "fish", "nu", "elvish", "xonsh"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Unknown,
    Working,
    Idle,
}

pub struct SpawnSpec {
    pub shell: String,
    pub cmd: Option<String>,
    pub hold: bool,
    pub cwd: PathBuf,
    pub title: Option<String>,
    pub env: Vec<(String, String)>,
    pub scrollback: usize,
    /// Clear the screen and scrollback once startup output settles (plain shells).
    pub clear_on_start: bool,
}

pub struct Pane {
    pub id: PaneId,
    pub title: Option<String>,
    pub parser: Arc<Mutex<Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Set on the first user input; stops the startup clear.
    typed: Arc<AtomicBool>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    size: (u16, u16),
    pub agent_state: AgentState,
    /// Set when a "working" pane goes idle; cleared when the pane is focused.
    pub unseen_finish: bool,
    /// Spawned automatically to fill a layout (Alt+l), not asked for.
    pub auto: bool,
}

fn nz(v: u16) -> NonZeroU16 {
    NonZeroU16::new(v.max(1)).unwrap()
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        spec: SpawnSpec,
        rows: u16,
        cols: u16,
        events: UnboundedSender<Event>,
        dirty: Arc<AtomicBool>,
    ) -> Result<Pane> {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let pair = native_pty_system()
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(&spec.shell);
        if let Some(c) = spec.cmd.as_deref().filter(|c| !c.trim().is_empty()) {
            let script = if spec.hold { format!("{c}; exec {}", spec.shell) } else { c.to_string() };
            cmd.arg("-c");
            cmd.arg(script);
        }
        let cwd = if spec.cwd.is_dir() { spec.cwd.clone() } else { dirs::home_dir().unwrap_or_default() };
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env_remove("TMUX");
        cmd.env_remove("TMUX_PANE");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }

        let child = pair.slave.spawn_command(cmd).context("spawning pane process")?;
        // Drop the slave so the reader sees EOF once the child exits.
        drop(pair.slave);

        let parser = Arc::new(Mutex::new(Parser::new_with_callbacks(
            nz(rows),
            nz(cols),
            spec.scrollback,
            TermCallbacks::default(),
        )));
        let writer = Arc::new(Mutex::new(pair.master.take_writer().context("pty writer")?));
        let mut reader = pair.master.try_clone_reader().context("pty reader")?;
        let typed = Arc::new(AtomicBool::new(false));
        let spawned = Instant::now();
        // Milliseconds after spawn of the latest output (0 = none yet).
        let last_output = Arc::new(AtomicU64::new(0));

        if spec.clear_on_start {
            let shell_pid = child.process_id();
            let (parser, writer, typed, last_output) =
                (parser.clone(), writer.clone(), typed.clone(), last_output.clone());
            std::thread::Builder::new().name(format!("pane-{id}-clear")).spawn(move || {
                // Wait for startup output (greeting, prompt) to go quiet.
                while spawned.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(50));
                    let last = last_output.load(Ordering::Acquire);
                    if last > 0 && spawned.elapsed().as_millis() as u64 >= last + 300 {
                        break;
                    }
                }
                if typed.load(Ordering::Acquire) {
                    return;
                }
                // Only shells with line editing treat Ctrl+L as "clear". In
                // dash or plain sh it would be typed into the next command.
                let editing = shell_pid
                    .and_then(procinfo::exe_name)
                    .is_some_and(|n| CLEARS_ON_CTRL_L.contains(&n.as_str()));
                if !editing {
                    return;
                }
                // Ctrl+L: every interactive shell clears and redraws its prompt;
                // nothing lands in the shell history.
                {
                    let mut w = writer.lock().unwrap();
                    let _ = w.write_all(b"\x0c");
                    let _ = w.flush();
                }
                std::thread::sleep(Duration::from_millis(200));
                parser.lock().unwrap().screen_mut().clear_scrollback();
            })?;
        }

        {
            let parser = parser.clone();
            let writer = writer.clone();
            let last_output = last_output.clone();
            std::thread::Builder::new()
                .name(format!("pane-{id}"))
                .spawn(move || {
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let ms = spawned.elapsed().as_millis().max(1) as u64;
                                last_output.store(ms, Ordering::Release);
                                let replies = {
                                    let mut p = parser.lock().unwrap();
                                    p.process(&buf[..n]);
                                    std::mem::take(&mut p.callbacks_mut().responses)
                                };
                                if !replies.is_empty() {
                                    let mut w = writer.lock().unwrap();
                                    let _ = w.write_all(&replies);
                                    let _ = w.flush();
                                }
                                // Coalesce redraw requests: one in flight at a time.
                                if !dirty.swap(true, Ordering::AcqRel) {
                                    let _ = events.send(Event::Redraw);
                                }
                            }
                        }
                    }
                    let _ = events.send(Event::PaneExited(id));
                })?;
        }

        Ok(Pane {
            id,
            title: spec.title,
            parser,
            writer,
            typed,
            master: pair.master,
            child,
            size: (rows, cols),
            agent_state: AgentState::Unknown,
            unseen_finish: false,
            auto: false,
        })
    }

    /// Nothing was ever typed, pasted or clicked into this pane.
    pub fn untouched(&self) -> bool {
        !self.typed.load(Ordering::Acquire)
    }

    pub fn write(&self, bytes: &[u8]) {
        self.typed.store(true, Ordering::Release);
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        self.parser.lock().unwrap().screen_mut().set_size(nz(rows), nz(cols));
    }

    pub fn child_pid(&self) -> Option<u32> {
        self.child.process_id()
    }

    /// Name of the PTY's foreground process (tmux's `pane_current_command`).
    pub fn current_command(&self) -> String {
        self.master
            .process_group_leader()
            .and_then(|pid| procinfo::comm(pid as u32))
            .or_else(|| self.child_pid().and_then(procinfo::comm))
            .unwrap_or_default()
    }

    pub fn cwd(&self) -> Option<PathBuf> {
        self.child_pid().and_then(procinfo::cwd)
    }

    pub fn osc_title(&self) -> Option<String> {
        self.parser.lock().unwrap().callbacks().title.clone().filter(|t| !t.is_empty())
    }

    pub fn scroll(&self, delta: isize) {
        let mut p = self.parser.lock().unwrap();
        let screen = p.screen_mut();
        let cur = screen.scrollback() as isize;
        screen.set_scrollback((cur + delta).max(0) as usize);
    }

    pub fn reset_scroll(&self) {
        let mut p = self.parser.lock().unwrap();
        if p.screen().scrollback() != 0 {
            p.screen_mut().set_scrollback(0);
        }
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn reap(&mut self) {
        let _ = self.child.try_wait();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait(); // SIGKILL: returns at once, and reaps
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_device_queries() {
        let mut p = Parser::new_with_callbacks(nz(24), nz(80), 0, TermCallbacks::default());
        p.process(b"hello\x1b[6n\x1b[c");
        let r = String::from_utf8(std::mem::take(&mut p.callbacks_mut().responses)).unwrap();
        assert_eq!(r, "\x1b[1;6R\x1b[?62;22c");
        p.process(b"\x1b]2;my title\x07");
        assert_eq!(p.callbacks().title.as_deref(), Some("my title"));
    }
}
