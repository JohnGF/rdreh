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
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // DECRQM: harnesses only bracket their frames in synchronized output
        // once the terminal says it knows the mode (2 = supported, now off).
        if (i1, i2, c, first_param(params)) == (Some(b'?'), Some(b'$'), 'p', 2026) {
            self.responses.extend_from_slice(b"\x1b[?2026;2$y");
            return;
        }
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

/// Feeds the parser, standing in for cursor sequences vt100 ignores:
/// - SCO save/restore (`CSI s`, `CSI u`) become DECSC/DECRC (`ESC 7`, `ESC 8`),
///   as xterm treats them. OpenCode saves the cursor, probes at the home
///   position, restores, then enters the alternate screen; without this the
///   restore was lost and the shell prompt came back at the top of the pane.
/// - Tab moves (`CSI n I` forward, `CSI n Z` back) become an absolute column.
///   agy moves left through its input line with backspaces plus `CSI n Z`, so
///   an edit mid-line was drawn at the wrong column until the next full redraw.
#[derive(Default)]
struct CursorFixups {
    /// An unfinished `ESC [ digits` held back until the next read.
    carry: Vec<u8>,
}

/// vt100 keeps a tab stop every 8 columns.
const TAB_WIDTH: u16 = 8;

impl CursorFixups {
    fn feed(&mut self, p: &mut Parser, input: &[u8]) {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(input);
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if data[i] != 0x1b {
                out.push(data[i]);
                i += 1;
                continue;
            }
            match data.get(i + 1) {
                None => {
                    self.carry = data[i..].to_vec();
                    break;
                }
                Some(b'[') => {}
                Some(_) => {
                    out.push(data[i]);
                    i += 1;
                    continue;
                }
            }
            let digits = data[i + 2..].iter().take_while(|b| b.is_ascii_digit()).count();
            let end = i + 2 + digits;
            let Some(&fin) = data.get(end) else {
                // Long digit runs are not ours to wait for.
                if digits < 6 {
                    self.carry = data[i..].to_vec();
                    break;
                }
                out.extend_from_slice(&data[i..]);
                break;
            };
            let n: u16 = std::str::from_utf8(&data[i + 2..end])
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
                .max(1);
            match (fin, digits) {
                (b's', 0) => out.extend_from_slice(b"\x1b7"),
                (b'u', 0) => out.extend_from_slice(b"\x1b8"),
                (b'I' | b'Z', _) => {
                    p.process(&std::mem::take(&mut out));
                    let screen = p.screen();
                    let (_, col) = screen.cursor_position();
                    let last = screen.size().1.get() - 1;
                    let col = if fin == b'I' {
                        (col / TAB_WIDTH).saturating_add(n).saturating_mul(TAB_WIDTH).min(last)
                    } else {
                        (col.saturating_sub(1) / TAB_WIDTH)
                            .saturating_sub(n - 1)
                            .saturating_mul(TAB_WIDTH)
                    };
                    out.extend_from_slice(format!("\x1b[{}G", col + 1).as_bytes());
                }
                _ => out.extend_from_slice(&data[i..=end]),
            }
            i = end + 1;
        }
        p.process(&out);
    }
}

/// How long a frame may stay open before redraws stop waiting for it, in case
/// the app died mid-frame and `CSI ?2026 l` never comes.
pub const SYNC_MAX_DEFER: Duration = Duration::from_millis(100);

/// Follows the synchronized-output bracket (`CSI ?2026 h/l`) across reads.
#[derive(Default)]
struct SyncTracker {
    /// End of the last read: a toggle split across two reads still matches.
    /// Shorter than a toggle, so it can never match twice.
    tail: Vec<u8>,
    in_sync: bool,
}

impl SyncTracker {
    const OPEN: &[u8] = b"\x1b[?2026h";
    const CLOSE: &[u8] = b"\x1b[?2026l";

    fn feed(&mut self, chunk: &[u8]) -> bool {
        let mut data = std::mem::take(&mut self.tail);
        data.extend_from_slice(chunk);
        let last = |pat: &[u8]| data.windows(pat.len()).rposition(|w| w == pat);
        match (last(Self::OPEN), last(Self::CLOSE)) {
            (Some(h), Some(l)) => self.in_sync = h > l,
            (Some(_), None) => self.in_sync = true,
            (None, Some(_)) => self.in_sync = false,
            (None, None) => {}
        }
        self.tail = data[data.len().saturating_sub(Self::OPEN.len() - 1)..].to_vec();
        self.in_sync
    }
}

/// Interactive shells whose line editor clears the screen on Ctrl+L.
const CLEARS_ON_CTRL_L: &[&str] = &["bash", "zsh", "fish", "nu", "elvish", "xonsh"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Unknown,
    Working,
    Idle,
}

pub const SHELLS: &[&str] = &["fish", "bash", "zsh", "sh", "dash", "nu", "elvish", "xonsh", ""];

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
    spawned: Instant,
    last_output: Arc<AtomicU64>,
    /// The app is between `CSI ?2026 h` and `l`: the screen holds half a frame.
    mid_frame: Arc<AtomicBool>,
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
        let mid_frame = Arc::new(AtomicBool::new(false));

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
            let mid_frame = mid_frame.clone();
            std::thread::Builder::new()
                .name(format!("pane-{id}"))
                .spawn(move || {
                    let mut buf = vec![0u8; 64 * 1024];
                    let mut fixups = CursorFixups::default();
                    let mut sync = SyncTracker::default();
                    let mut frame_start = Instant::now();
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let ms = spawned.elapsed().as_millis().max(1) as u64;
                                last_output.store(ms, Ordering::Release);
                                let was_in_sync = sync.in_sync;
                                let in_sync = sync.feed(&buf[..n]);
                                if in_sync && !was_in_sync {
                                    frame_start = Instant::now();
                                }
                                // Give up on a frame that never closes.
                                let in_sync = in_sync && frame_start.elapsed() < SYNC_MAX_DEFER;
                                sync.in_sync = in_sync;
                                // Raised before the parser sees the frame and
                                // lowered after, to keep draws out of it.
                                if in_sync {
                                    mid_frame.store(true, Ordering::Release);
                                }
                                let replies = {
                                    let mut p = parser.lock().unwrap();
                                    fixups.feed(&mut p, &buf[..n]);
                                    std::mem::take(&mut p.callbacks_mut().responses)
                                };
                                if !replies.is_empty() {
                                    let mut w = writer.lock().unwrap();
                                    let _ = w.write_all(&replies);
                                    let _ = w.flush();
                                }
                                if in_sync {
                                    continue; // The closing `l` asks for the redraw.
                                }
                                mid_frame.store(false, Ordering::Release);
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
            spawned,
            last_output,
            mid_frame,
            master: pair.master,
            child,
            size: (rows, cols),
            agent_state: AgentState::Unknown,
            unseen_finish: false,
            auto: false,
        })
    }

    /// Produced output within the last `window` (for agents that report no state).
    pub fn recently_active(&self, window: Duration) -> bool {
        let last = self.last_output.load(Ordering::Acquire);
        last > 0 && (self.spawned.elapsed().as_millis() as u64) < last + window.as_millis() as u64
    }

    /// Mid-way through a synchronized frame: drawing now would show half of it.
    pub fn mid_frame(&self) -> bool {
        self.mid_frame.load(Ordering::Acquire)
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
        let leader = self.master.process_group_leader().map(|p| p as u32).or_else(|| self.child_pid());
        let Some(leader) = leader else { return String::new() };
        let cmd = procinfo::comm(leader).unwrap_or_default();
        // A pane started with `cmd = ...` runs `<shell> -c "cmd; exec <shell>"`.
        // Without job control the command shares the shell's group, so look
        // past the shell to what it is running.
        if SHELLS.contains(&cmd.as_str())
            && let Some(child) = procinfo::same_group_child(leader)
            && let Some(c) = procinfo::comm(child)
        {
            return c;
        }
        cmd
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

    /// Extracts text from the terminal buffer between two absolute row coordinates.
    pub fn extract_text(
        &self,
        start_abs_row: usize,
        start_col: u16,
        end_abs_row: usize,
        end_col: u16,
    ) -> String {
        let mut p = self.parser.lock().unwrap();
        extract_screen_text(p.screen_mut(), start_abs_row, start_col, end_abs_row, end_col)
    }
}

/// Extracts text from a vt100 screen buffer between two absolute row coordinates.
pub fn extract_screen_text(
    screen: &mut vt100::Screen,
    start_abs_row: usize,
    start_col: u16,
    end_abs_row: usize,
    end_col: u16,
) -> String {
    let (sr, sc, er, ec) = if start_abs_row < end_abs_row
        || (start_abs_row == end_abs_row && start_col <= end_col)
    {
        (start_abs_row, start_col, end_abs_row, end_col)
    } else {
        (end_abs_row, end_col, start_abs_row, start_col)
    };

    let saved_sb = screen.scrollback();
    let total_history = screen.scrollback_len();
    let (rows, cols) = {
        let (r, c) = screen.size();
        (r.get(), c.get())
    };

    let mut result = String::new();
    for r_abs in sr..=er {
        let (vis_row, sb_needed) = if r_abs < total_history {
            (0, total_history - r_abs)
        } else {
            (r_abs - total_history, 0)
        };
        if vis_row >= rows as usize {
            break;
        }
        screen.set_scrollback(sb_needed);
        let vis_row_u16 = vis_row as u16;

        let col_from = if r_abs == sr { sc } else { 0 };
        let col_to = if r_abs == er { ec } else { cols.saturating_sub(1) };

        let mut line = String::new();
        for c in col_from..=col_to {
            if let Some(cell) = screen.cell(vis_row_u16, c)
                && !cell.is_wide_continuation()
            {
                let contents = cell.contents();
                line.push_str(if contents.is_empty() { " " } else { contents });
            }
        }

        let wrapped = screen.row_wrapped(vis_row_u16);
        if r_abs < er {
            let trimmed = line.trim_end();
            result.push_str(trimmed);
            if !wrapped {
                result.push('\n');
            }
        } else {
            let trimmed = line.trim_end();
            result.push_str(trimmed);
        }
    }

    screen.set_scrollback(saved_sb);
    result
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

    #[test]
    fn reports_synchronized_output_support() {
        let mut p = Parser::new_with_callbacks(nz(24), nz(80), 0, TermCallbacks::default());
        p.process(b"\x1b[?2026$p\x1b[?2027$p");
        assert_eq!(p.callbacks().responses, b"\x1b[?2026;2$y");
    }

    #[test]
    fn sync_tracker_follows_toggles_split_across_reads() {
        let mut t = SyncTracker::default();
        let feed_bytes = |t: &mut SyncTracker, s: &[u8]| {
            s.iter().map(|b| t.feed(std::slice::from_ref(b))).last().unwrap()
        };
        assert!(feed_bytes(&mut t, b"x\x1b[?2026h"));
        assert!(t.feed(b"half a frame"));
        assert!(!feed_bytes(&mut t, b"\x1b[?2026l"));
        // The last toggle in a read wins: this one ends inside the next frame.
        assert!(t.feed(b"\x1b[?2026h1\x1b[?2026l\x1b[?2026h2"));
        assert!(!t.feed(b"\x1b[?2026l\x1b[?2026h3\x1b[?2026l"));
        assert!(!t.feed(b"plain output"));
    }

    #[test]
    fn sco_save_restore_survives_alt_screen() {
        // OpenCode's startup/exit, fed one byte at a time to cover split reads.
        let stream: &[u8] = b"$ opencode\r\n\x1b[s\x1b[H\x1b[6n\x1b[u\x1b[s\x1b[?1049h\x1b[Hjunk\x1b[?1049l$ ";
        let mut p = Parser::new_with_callbacks(nz(24), nz(80), 0, TermCallbacks::default());
        let mut f = CursorFixups::default();
        for b in stream {
            f.feed(&mut p, std::slice::from_ref(b));
        }
        assert_eq!(p.screen().cursor_position(), (1, 2));
        // Kitty keyboard flags (`CSI ? u`, `CSI > 1 u`) are not a restore.
        f.feed(&mut p, b"\x1b[3;5H\x1b[?u\x1b[>1u");
        assert_eq!(p.screen().cursor_position(), (2, 4));
    }

    #[test]
    fn tab_moves_land_on_tab_stops() {
        let mut p = Parser::new_with_callbacks(nz(24), nz(80), 0, TermCallbacks::default());
        let mut f = CursorFixups::default();
        let mut at = |p: &mut Parser, s: &[u8]| {
            for b in s {
                f.feed(p, std::slice::from_ref(b));
            }
            p.screen().cursor_position().1
        };
        assert_eq!(at(&mut p, b"\x1b[1;14H\x1b[Z"), 8);
        assert_eq!(at(&mut p, b"\x1b[Z"), 0);
        assert_eq!(at(&mut p, b"\x1b[Z"), 0);
        assert_eq!(at(&mut p, b"\x1b[1;30H\x1b[2Z"), 16);
        assert_eq!(at(&mut p, b"\x1b[I"), 24);
        assert_eq!(at(&mut p, b"\x1b[3I"), 48);
        assert_eq!(at(&mut p, b"\x1b[99I"), 79);
        // agy moving 13 left from the end of its input line, then inserting.
        let line = b"\r> the quick brown fox jumps over the lazy dog";
        assert_eq!(at(&mut p, line), 45);
        at(&mut p, b"\x08\x1b[2ZV the lazy dog\x1b[13D");
        let row = p.screen().contents().lines().next().unwrap().trim_end().to_string();
        assert_eq!(row, "> the quick brown fox jumps overV the lazy dog");
    }

    #[test]
    fn extracts_screen_text_single_and_multi_line() {
        let mut p = Parser::new_with_callbacks(nz(24), nz(80), 100, TermCallbacks::default());
        p.process(b"hello world\r\nline two\r\nline three");
        let screen = p.screen_mut();
        assert_eq!(extract_screen_text(screen, 0, 0, 0, 4), "hello");
        assert_eq!(extract_screen_text(screen, 0, 6, 0, 10), "world");
        assert_eq!(extract_screen_text(screen, 0, 0, 1, 3), "hello world\nline");
    }

    #[test]
    fn extracts_screen_text_with_scrollback() {
        let mut p = Parser::new_with_callbacks(nz(5), nz(80), 100, TermCallbacks::default());
        for i in 1..=10 {
            p.process(format!("line {i}\r\n").as_bytes());
        }
        let screen = p.screen_mut();
        assert!(screen.scrollback_len() >= 5);
        let first = extract_screen_text(screen, 0, 0, 0, 80);
        assert_eq!(first, "line 1");
    }
}
