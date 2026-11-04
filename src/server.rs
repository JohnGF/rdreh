//! The rdreh server: owns every pane and keeps running when no terminal is
//! attached. A client attaches over the Unix socket, sends its input events,
//! and receives rendered frames (ANSI) for its own terminal size.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::Event as TermEvent;
use crossterm::{queue, terminal::SetTitle};
use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Rect, Size};
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc::UnboundedSender;

use crate::app::{App, Event};
use crate::config::Config;
use crate::desktop::Desktop;
use crate::ipc;
use crate::layout::Layout;
use crate::ui;

/// Written just before the server closes a client's stream, so the client
/// can tell "detached" from "rdreh exited". Unknown OSCs are ignored by
/// terminals, so it is invisible if it ever reaches one.
pub const MARK_DETACHED: &[u8] = b"\x1b]7777;rdreh:detached\x07";
pub const MARK_EXITED: &[u8] = b"\x1b]7777;rdreh:exited\x07";

/// Byte sink shared between the crossterm backend and the frame shipper.
#[derive(Clone, Default)]
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Renders into a byte buffer at the attached client's size. The server has
/// no terminal of its own, so size and cursor queries answer from state.
pub struct RemoteBackend {
    inner: CrosstermBackend<SharedBuf>,
    out: SharedBuf,
    size: Size,
    cursor: Position,
}

impl RemoteBackend {
    fn new(size: Size) -> Self {
        let out = SharedBuf::default();
        Self { inner: CrosstermBackend::new(out.clone()), out, size, cursor: Position::ORIGIN }
    }
    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.out.0.lock().unwrap())
    }
    fn buf(&mut self) -> SharedBuf {
        self.out.clone()
    }
}

impl Backend for RemoteBackend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.cursor = position.into();
        self.inner.set_cursor_position(self.cursor)
    }
    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> io::Result<Size> {
        Ok(self.size)
    }
    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize { columns_rows: self.size, pixels: Size::new(0, 0) })
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn cursor_style(s: vt100::CursorStyle) -> SetCursorStyle {
    use vt100::CursorStyle as C;
    match s {
        C::Default => SetCursorStyle::DefaultUserShape,
        C::BlinkBlock => SetCursorStyle::BlinkingBlock,
        C::SteadyBlock => SetCursorStyle::SteadyBlock,
        C::BlinkUnderline => SetCursorStyle::BlinkingUnderScore,
        C::SteadyUnderline => SetCursorStyle::SteadyUnderScore,
        C::BlinkVerticalBar => SetCursorStyle::BlinkingBar,
        C::SteadyVerticalBar => SetCursorStyle::SteadyBar,
    }
}

struct Client {
    id: u64,
    out: UnboundedSender<Vec<u8>>,
    terminal: Terminal<RemoteBackend>,
    last_title: String,
    last_cursor: Option<vt100::CursorStyle>,
}

impl Client {
    fn new(id: u64, cols: u16, rows: u16, out: UnboundedSender<Vec<u8>>) -> io::Result<Client> {
        let area = Rect::new(0, 0, cols.max(1), rows.max(1));
        let terminal = Terminal::with_options(
            RemoteBackend::new(area.as_size()),
            TerminalOptions { viewport: Viewport::Fixed(area) },
        )?;
        let mut c = Client { id, out, terminal, last_title: String::new(), last_cursor: None };
        c.terminal.clear()?; // fresh client: next draw repaints everything
        Ok(c)
    }

    fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        let area = Rect::new(0, 0, cols.max(1), rows.max(1));
        self.terminal.backend_mut().size = area.as_size();
        self.terminal.resize(area)?;
        self.terminal.clear()
    }

    /// Render and ship one frame. False when the client is gone.
    fn draw(&mut self, app: &mut App) -> bool {
        let mut style = None;
        if self.terminal.draw(|f| style = ui::draw(f, app)).is_err() {
            return false;
        }
        let backend = self.terminal.backend_mut();
        if let Some(s) = style
            && self.last_cursor != Some(s)
        {
            let _ = queue!(backend.buf(), cursor_style(s));
            self.last_cursor = Some(s);
        }
        let title = app.window_title();
        if title != self.last_title {
            let _ = queue!(backend.buf(), SetTitle(&title));
            self.last_title = title;
        }
        for payload in app.pending_clipboard.drain(..) {
            use std::io::Write;
            let _ = backend.buf().write_all(&payload);
        }
        let mut bytes = backend.take();
        if bytes.is_empty() {
            return true;
        }
        // Synchronized output: the outer terminal applies the whole frame
        // at once, so large redraws (long input boxes scrolling/wrapping)
        // never show half-painted. Unsupporting terminals ignore it.
        bytes.splice(0..0, b"\x1b[?2026h".iter().copied());
        bytes.extend_from_slice(b"\x1b[?2026l");
        self.out.send(bytes).is_ok()
    }

    fn close(self, mark: &[u8]) {
        let _ = self.out.send(mark.to_vec());
    }
}

pub struct ServerArgs {
    pub cols: u16,
    pub rows: u16,
    pub dirs: Vec<PathBuf>,
    pub layout: Option<Layout>,
}

pub async fn run(cfg: Config, session: String, args: ServerArgs) -> Result<()> {
    let socket = ipc::socket_path(&session);
    let listener = ipc::bind(&socket)?;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let desktop = Desktop::start(cfg.general.badge_app_uris.clone(), cfg.general.desktop_entry.clone());
    let launch_cwd = std::env::current_dir().unwrap_or_default();
    let size = if args.cols >= 20 && args.rows >= 5 { (args.cols, args.rows) } else { (160, 48) };
    let mut app = App::new(cfg, size, tx.clone(), desktop, socket.clone());
    app.bootstrap(&launch_cwd, &args.dirs, args.layout);
    ipc::serve(listener, tx);

    let mut client: Option<Client> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let frame_budget = Duration::from_millis(8);
    let mut last_draw = Instant::now() - frame_budget;
    let mut needs_draw = false;

    loop {
        if needs_draw && last_draw.elapsed() >= frame_budget {
            app.dirty.store(false, Ordering::Release);
            if let Some(c) = client.as_mut()
                && !c.draw(&mut app)
            {
                client = None;
            }
            last_draw = Instant::now();
            needs_draw = false;
        }

        let wait = frame_budget.saturating_sub(last_draw.elapsed());
        tokio::select! {
            Some(ev) = rx.recv() => {
                handle(&mut app, &mut client, ev);
                while let Ok(ev) = rx.try_recv() {
                    handle(&mut app, &mut client, ev);
                }
                needs_draw = client.is_some();
            }
            _ = tick.tick() => {
                app.refresh_tracking();
                needs_draw = client.is_some();
            }
            _ = tokio::time::sleep(wait), if needs_draw => {}
        }

        if app.detach_requested {
            app.detach_requested = false;
            if let Some(c) = client.take() {
                c.close(MARK_DETACHED);
            }
        }
        if app.quit {
            break;
        }
    }

    // Tear everything down first, so "exited" means the panes are gone.
    let _ = std::fs::remove_file(&socket);
    let attached = client.take();
    app.shutdown().await;
    if let Some(c) = attached {
        c.close(MARK_EXITED);
    }
    // Let the writer task flush the exit mark before the runtime stops.
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

fn handle(app: &mut App, client: &mut Option<Client>, ev: Event) {
    match ev {
        Event::Redraw => {}
        Event::PaneExited(id) => app.pane_exited(id),
        Event::Ipc(req, pid, reply) => {
            let resp = app.handle_ipc(req, pid);
            let _ = reply.send(resp);
        }
        Event::Attach { id, cols, rows, out } => {
            // One attached terminal at a time: a new attach takes over.
            if let Some(old) = client.take() {
                old.close(MARK_DETACHED);
            }
            match Client::new(id, cols, rows, out) {
                Ok(c) => {
                    app.resize(cols, rows);
                    *client = Some(c);
                }
                Err(e) => eprintln!("rdreh: attach failed: {e}"),
            }
        }
        Event::Input { id, ev } => {
            let Some(c) = client.as_mut().filter(|c| c.id == id) else { return };
            if let TermEvent::Resize(w, h) = ev
                && c.resize(w, h).is_err()
            {
                *client = None;
                return;
            }
            app.handle_term_event(ev);
        }
        Event::Detached { id } => {
            if client.as_ref().is_some_and(|c| c.id == id) {
                *client = None;
            }
        }
    }
}
