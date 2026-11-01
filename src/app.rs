//! Application state and every user-visible action. The event loop in
//! `main.rs` feeds terminal input, pane output, IPC requests and ticks here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use serde_json::json;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::config::{Config, PaneConfig, WorkspaceConfig, expand_tilde};
use crate::desktop::Desktop;
use crate::ipc::{Request, Response};
use crate::keys;
use crate::layout::{self, Dir, Layout};
use crate::pane::{AgentState, Pane, PaneId, SpawnSpec};
use crate::procinfo;

pub enum Event {
    /// Some pane produced output.
    Redraw,
    PaneExited(PaneId),
    Ipc(Request, Option<u32>, oneshot::Sender<Response>),
    /// A terminal attached; frames for it go to `out`.
    Attach { id: u64, cols: u16, rows: u16, out: tokio::sync::mpsc::UnboundedSender<Vec<u8>> },
    /// Input from the attached terminal `id`.
    Input { id: u64, ev: TermEvent },
    /// Terminal `id` disconnected.
    Detached { id: u64 },
}

pub struct Workspace {
    pub index: u8,
    pub name: String,
    pub layout: Layout,
    pub panes: Vec<PaneId>,
    pub focus: usize,
    pub zoomed: bool,
    pub tracked: bool,
}

impl Workspace {
    pub fn focused(&self) -> Option<PaneId> {
        self.panes.get(self.focus).copied()
    }
    /// Only the focused pane is drawn (Single layout or zoom).
    pub fn monocle(&self) -> bool {
        self.zoomed || self.layout == Layout::Single
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    AddProject,
    Rename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Quit,
    CloseWorkspace,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Prefix,
    Help,
    Prompt { kind: PromptKind, input: String },
    Confirm(ConfirmKind),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub total: usize,
    pub busy: usize,
    pub finished: usize,
}

pub struct App {
    pub cfg: Config,
    pub panes: HashMap<PaneId, Pane>,
    pub workspaces: Vec<Workspace>,
    pub active: usize,
    pub mode: Mode,
    pub message: Option<(String, Instant)>,
    pub notifs: bool,
    pub badge: bool,
    pub agents_only: bool,
    pub counts: Counts,
    pub quit: bool,
    /// Ctrl+b d: drop the attached terminal, keep everything running.
    pub detach_requested: bool,
    pub dirty: Arc<AtomicBool>,
    /// Hit-testing data from the last draw.
    pub visible: Vec<(PaneId, Rect)>,
    pub tab_hits: Vec<(u8, Rect)>,
    commands: HashMap<PaneId, String>,
    next_id: PaneId,
    term_size: (u16, u16),
    events: UnboundedSender<Event>,
    desktop: Desktop,
    socket: PathBuf,
    exe: String,
}

pub fn inner(r: Rect) -> Rect {
    Rect::new(
        r.x.saturating_add(1),
        r.y.saturating_add(1),
        r.width.saturating_sub(2),
        r.height.saturating_sub(2),
    )
}

const SHELLS: &[&str] = &["fish", "bash", "zsh", "sh", "dash", "nu", "elvish", "xonsh", ""];

impl App {
    pub fn new(
        cfg: Config,
        term_size: (u16, u16),
        events: UnboundedSender<Event>,
        desktop: Desktop,
        socket: PathBuf,
    ) -> App {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "rdr".into());
        App {
            notifs: cfg.general.notification_popups,
            badge: cfg.general.taskbar_badge,
            agents_only: cfg.agents_only(),
            cfg,
            panes: HashMap::new(),
            workspaces: Vec::new(),
            active: 0,
            mode: Mode::Normal,
            message: None,
            counts: Counts::default(),
            quit: false,
            detach_requested: false,
            dirty: Arc::new(AtomicBool::new(false)),
            visible: Vec::new(),
            tab_hits: Vec::new(),
            commands: HashMap::new(),
            next_id: 1,
            term_size,
            events,
            desktop,
            socket,
            exe,
        }
    }

    /// Kill every pane, clear the badge and flush D-Bus.
    pub async fn shutdown(self) {
        let App { panes, desktop, .. } = self;
        drop(panes);
        desktop.shutdown().await;
    }

    // ---------------------------------------------------------------- setup

    pub fn bootstrap(&mut self, launch_cwd: &Path, extra_dirs: &[PathBuf], dir_layout: Option<Layout>) {
        let configured: Vec<(u8, WorkspaceConfig)> = self
            .cfg
            .workspaces
            .iter()
            .filter_map(|(k, v)| Some((k.parse().ok()?, v.clone())))
            .collect();
        for (index, wc) in configured {
            let layout = wc.layout.unwrap_or(Layout::Single);
            let ws_cwd = wc.cwd.as_deref().map(expand_tilde).unwrap_or_else(|| launch_cwd.to_path_buf());
            let specs: Vec<PaneConfig> = if wc.panes.is_empty() {
                let n = wc.count.unwrap_or_else(|| layout.min_panes()).max(1);
                vec![PaneConfig { cmd: wc.cmd.clone(), ..Default::default() }; n]
            } else {
                wc.panes.clone()
            };
            let name = wc.name.clone().unwrap_or_else(|| format!("ws{index}"));
            self.create_workspace(index, name, layout, wc.tracked.unwrap_or(true), &ws_cwd, specs);
        }
        let start = self.cfg.general.start_workspace;
        if let Some(pos) = start.and_then(|s| self.workspaces.iter().position(|w| w.index == s)) {
            self.active = pos;
        }
        for dir in extra_dirs {
            self.add_project(dir, dir_layout);
        }
        if self.workspaces.is_empty() {
            let cwd = launch_cwd.to_path_buf();
            self.create_workspace(1, "shell".into(), Layout::Single, true, &cwd, vec![PaneConfig::default()]);
        }
        self.relayout();
        self.refresh_tracking();
    }

    fn free_index(&self) -> Option<u8> {
        (1..=9).chain([0]).find(|i| !self.workspaces.iter().any(|w| w.index == *i))
    }

    fn create_workspace(
        &mut self,
        index: u8,
        name: String,
        layout: Layout,
        tracked: bool,
        cwd: &Path,
        specs: Vec<PaneConfig>,
    ) -> usize {
        let area = self.workspace_area();
        let rects = layout::compute(layout, specs.len(), area);
        let mut ids = Vec::new();
        for (spec, rect) in specs.into_iter().zip(rects) {
            let pane_cwd = spec.cwd.as_deref().map(expand_tilde).unwrap_or_else(|| cwd.to_path_buf());
            if let Some(id) = self.spawn(spec.cmd, spec.hold, pane_cwd, spec.title, rect) {
                ids.push(id);
            }
        }
        let ws = Workspace { index, name, layout, panes: ids, focus: 0, zoomed: false, tracked };
        let pos = self.workspaces.partition_point(|w| w.index < index);
        self.workspaces.insert(pos, ws);
        pos
    }

    fn spawn(&mut self, cmd: Option<String>, hold: bool, cwd: PathBuf, title: Option<String>, rect: Rect) -> Option<PaneId> {
        let id = self.next_id;
        self.next_id += 1;
        let clear_on_start = self.cfg.general.clear_on_start && cmd.as_deref().is_none_or(|c| c.trim().is_empty());
        let spec = SpawnSpec {
            shell: self.cfg.general.default_shell.clone(),
            clear_on_start,
            cmd,
            hold,
            cwd,
            title,
            env: vec![
                ("RDR_PANE".into(), id.to_string()),
                ("RDR_SOCKET".into(), self.socket.display().to_string()),
                ("RDR_BIN".into(), self.exe.clone()),
                ("RDR_SESSION".into(), self.cfg.general.session_name.clone()),
            ],
            scrollback: self.cfg.general.scrollback,
        };
        let r = inner(rect);
        match Pane::spawn(id, spec, r.height, r.width, self.events.clone(), self.dirty.clone()) {
            Ok(p) => {
                self.panes.insert(id, p);
                Some(id)
            }
            Err(e) => {
                self.flash(format!("spawn failed: {e:#}"));
                None
            }
        }
    }

    // --------------------------------------------------------------- layout

    pub fn workspace_area(&self) -> Rect {
        let (w, h) = self.term_size;
        Rect::new(0, 0, w, h.saturating_sub(1))
    }

    pub fn resize(&mut self, w: u16, h: u16) {
        self.term_size = (w, h);
        self.relayout();
    }

    /// Outer rects for every pane of a workspace (hidden ones included).
    pub fn pane_rects(&self, ws: &Workspace) -> Vec<Rect> {
        let area = self.workspace_area();
        let mut rects = layout::compute(ws.layout, ws.panes.len(), area);
        if ws.zoomed && let Some(r) = rects.get_mut(ws.focus) {
            *r = area;
        }
        rects
    }

    pub fn visible_panes(&self, ws: &Workspace) -> Vec<(PaneId, Rect)> {
        let rects = self.pane_rects(ws);
        if ws.monocle() {
            ws.focused().map(|id| vec![(id, rects[ws.focus])]).unwrap_or_default()
        } else {
            ws.panes.iter().copied().zip(rects).collect()
        }
    }

    pub fn relayout(&mut self) {
        let mut sizes = Vec::new();
        for ws in &self.workspaces {
            for (id, rect) in ws.panes.iter().zip(self.pane_rects(ws)) {
                sizes.push((*id, inner(rect)));
            }
        }
        for (id, r) in sizes {
            if let Some(p) = self.panes.get_mut(&id) {
                p.resize(r.height, r.width);
            }
        }
    }

    pub fn ws(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active)
    }

    fn ws_mut(&mut self) -> Option<&mut Workspace> {
        self.workspaces.get_mut(self.active)
    }

    pub fn focused_pane(&self) -> Option<&Pane> {
        self.ws().and_then(Workspace::focused).and_then(|id| self.panes.get(&id))
    }

    fn focused_cwd(&self) -> PathBuf {
        self.focused_pane()
            .and_then(Pane::cwd)
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default()
    }

    pub fn flash(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    pub fn current_message(&self) -> Option<&str> {
        self.message
            .as_ref()
            .filter(|(_, t)| t.elapsed() < Duration::from_secs(3))
            .map(|(m, _)| m.as_str())
    }

    fn on_focus_changed(&mut self) {
        if let Some(id) = self.ws().and_then(Workspace::focused)
            && let Some(p) = self.panes.get_mut(&id)
        {
            p.unseen_finish = false;
        }
    }

    pub fn switch_workspace(&mut self, index: u8) {
        if let Some(pos) = self.workspaces.iter().position(|w| w.index == index) {
            self.active = pos;
            self.on_focus_changed();
        } else {
            self.flash(format!("No workspace {index}"));
        }
    }

    fn step_workspace(&mut self, forward: bool) {
        let n = self.workspaces.len();
        if n > 1 {
            self.active = if forward { (self.active + 1) % n } else { (self.active + n - 1) % n };
            self.on_focus_changed();
        }
    }

    fn focus_dir(&mut self, dir: Dir) {
        let Some(ws) = self.ws() else { return };
        let (n, focus, monocle) = (ws.panes.len(), ws.focus, ws.monocle());
        let target = if monocle {
            // One visible pane: left/right walk the hidden ones in order.
            match dir {
                Dir::Left if focus > 0 => Some(focus - 1),
                Dir::Right if focus + 1 < n => Some(focus + 1),
                _ => None,
            }
        } else {
            layout::neighbor(&self.pane_rects(ws), focus, dir)
        };
        match (target, dir) {
            (Some(i), _) => {
                if let Some(ws) = self.ws_mut() {
                    ws.focus = i;
                }
                self.on_focus_changed();
            }
            // At the edge, Shift+Left/Right wrap to the previous/next workspace.
            (None, Dir::Left) => self.step_workspace(false),
            (None, Dir::Right) => self.step_workspace(true),
            _ => {}
        }
    }

    fn cycle_pane(&mut self) {
        if let Some(ws) = self.ws_mut()
            && !ws.panes.is_empty()
        {
            ws.focus = (ws.focus + 1) % ws.panes.len();
        }
        self.on_focus_changed();
    }

    pub fn set_layout(&mut self, layout: Layout) {
        let Some(ws) = self.ws() else { return };
        let missing = layout.min_panes().saturating_sub(ws.panes.len());
        let cwd = self.focused_cwd();
        for _ in 0..missing {
            let rect = self.workspace_area();
            if let Some(id) = self.spawn(None, true, cwd.clone(), None, rect)
                && let Some(ws) = self.ws_mut()
            {
                ws.panes.push(id);
                if let Some(p) = self.panes.get_mut(&id) {
                    p.auto = true;
                }
            }
        }
        if matches!(layout, Layout::Single | Layout::Dual | Layout::Tri) {
            self.reap_layout_fillers(layout.min_panes());
        }
        if let Some(ws) = self.ws_mut() {
            ws.layout = layout;
            ws.zoomed = false;
        }
        self.relayout();
        self.flash(format!("Layout: {} [Alt+l to cycle]", layout.name()));
    }

    /// Shrinking back after Alt+l: close panes that a layout change spawned
    /// and that were never used (no input, still at the shell prompt), until
    /// `keep` panes remain. The focused pane and anything used stay.
    fn reap_layout_fillers(&mut self, keep: usize) {
        let Some(ws) = self.ws() else { return };
        let focused = ws.focused();
        let excess = ws.panes.len().saturating_sub(keep.max(1));
        let doomed: Vec<PaneId> = ws
            .panes
            .iter()
            .rev()
            .copied()
            .filter(|id| Some(*id) != focused)
            .filter(|id| {
                self.panes.get(id).is_some_and(|p| {
                    p.auto && p.untouched() && SHELLS.contains(&p.current_command().as_str())
                })
            })
            .take(excess)
            .collect();
        if doomed.is_empty() {
            return;
        }
        for id in &doomed {
            self.panes.remove(id); // dropping a Pane kills its process
            self.commands.remove(id);
        }
        if let Some(ws) = self.ws_mut() {
            ws.panes.retain(|id| !doomed.contains(id));
            ws.focus = focused.and_then(|f| ws.panes.iter().position(|id| *id == f)).unwrap_or(0);
        }
    }

    fn cycle_layout(&mut self) {
        if let Some(ws) = self.ws() {
            let cur = if ws.zoomed { Layout::Single } else { ws.layout };
            self.set_layout(cur.next());
        }
    }

    fn toggle_zoom(&mut self) {
        if let Some(ws) = self.ws_mut() {
            ws.zoomed = !ws.zoomed;
        }
        self.relayout();
    }

    fn reset_layout(&mut self) {
        if let Some(ws) = self.ws_mut() {
            ws.zoomed = false;
        }
        self.relayout();
        let name = self.ws().map(|w| w.layout.name()).unwrap_or("");
        self.flash(format!("Panes restored: {name}"));
    }

    fn add_pane(&mut self) {
        let cwd = self.focused_cwd();
        let rect = self.workspace_area();
        if let Some(id) = self.spawn(None, true, cwd, None, rect)
            && let Some(ws) = self.ws_mut()
        {
            ws.panes.push(id);
            ws.focus = ws.panes.len() - 1;
            ws.zoomed = false;
            if ws.layout == Layout::Single {
                ws.layout = Layout::Dual;
            }
        }
        self.relayout();
    }

    fn close_pane(&mut self) {
        if let Some(id) = self.ws().and_then(Workspace::focused)
            && let Some(p) = self.panes.get_mut(&id)
        {
            p.kill(); // removal happens when the reader reports exit
        }
    }

    fn close_workspace(&mut self) {
        if let Some(ws) = self.ws() {
            for id in ws.panes.clone() {
                if let Some(p) = self.panes.get_mut(&id) {
                    p.kill();
                }
            }
        }
    }

    pub fn add_project(&mut self, dir: &Path, layout: Option<Layout>) -> Option<u8> {
        let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if !dir.is_dir() {
            self.flash(format!("Not a directory: {}", dir.display()));
            return None;
        }
        let Some(index) = self.free_index() else {
            self.flash("All 10 workspaces are in use");
            return None;
        };
        let layout = layout.unwrap_or(self.cfg.general.default_layout);
        let name = dir.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "project".into());
        let specs = vec![PaneConfig::default(); layout.min_panes()];
        self.active = self.create_workspace(index, name, layout, true, &dir, specs);
        self.relayout();
        Some(index)
    }

    pub fn pane_exited(&mut self, id: PaneId) {
        if let Some(mut p) = self.panes.remove(&id) {
            p.reap();
        }
        self.commands.remove(&id);
        for ws in &mut self.workspaces {
            if let Some(i) = ws.panes.iter().position(|p| *p == id) {
                ws.panes.remove(i);
                if ws.focus > i || ws.focus >= ws.panes.len() {
                    ws.focus = ws.focus.saturating_sub(1);
                }
                if ws.panes.len() <= 1 {
                    ws.zoomed = false;
                }
            }
        }
        let before = self.workspaces.len();
        let active_index = self.ws().map(|w| w.index);
        self.workspaces.retain(|w| !w.panes.is_empty());
        if self.workspaces.is_empty() {
            self.quit = true;
            return;
        }
        if self.workspaces.len() != before {
            self.active = active_index
                .and_then(|i| self.workspaces.iter().position(|w| w.index == i))
                .unwrap_or_else(|| self.active.min(self.workspaces.len() - 1));
        }
        self.relayout();
        self.refresh_tracking();
    }

    // ------------------------------------------------------------- tracking

    fn is_agent_command(&self, cmd: &str) -> bool {
        // Claude Code's process title is its version number (e.g. "2.1.3").
        let versionish = cmd.chars().next().is_some_and(|c| c.is_ascii_digit()) && cmd.contains('.');
        versionish || self.cfg.general.agent_commands.iter().any(|a| cmd.starts_with(a.as_str()))
    }

    /// Is this pane currently an agent (process match, or it reported state)?
    pub fn pane_is_agent(&self, id: PaneId) -> bool {
        let cmd = self.commands.get(&id).map(String::as_str).unwrap_or("");
        if SHELLS.contains(&cmd) {
            return false;
        }
        let reported = self.panes.get(&id).is_some_and(|p| p.agent_state != AgentState::Unknown);
        reported || self.is_agent_command(cmd)
    }

    fn workspace_counts(&self, ws: &Workspace) -> bool {
        if self.agents_only {
            ws.name == "agents"
        } else {
            ws.tracked
        }
    }

    pub fn compute_counts(&self) -> Counts {
        let mut c = Counts::default();
        for ws in self.workspaces.iter().filter(|w| self.workspace_counts(w)) {
            for id in &ws.panes {
                if !self.pane_is_agent(*id) {
                    continue;
                }
                c.total += 1;
                if self.panes.get(id).is_some_and(|p| p.agent_state == AgentState::Working) {
                    c.busy += 1;
                } else {
                    c.finished += 1;
                }
            }
        }
        c
    }

    pub fn window_title(&self) -> String {
        if self.counts.total == 0 {
            "rdreh".into()
        } else {
            format!("[Finished: {}/{}] rdreh", self.counts.finished, self.counts.total)
        }
    }

    /// Re-read foreground commands, recount, push the badge.
    pub fn refresh_tracking(&mut self) {
        self.commands = self.panes.iter().map(|(id, p)| (*id, p.current_command())).collect();
        // A pane whose agent exited back to the shell is no longer "finished".
        for (id, cmd) in &self.commands {
            if SHELLS.contains(&cmd.as_str())
                && let Some(p) = self.panes.get_mut(id)
                && p.agent_state == AgentState::Idle
            {
                p.agent_state = AgentState::Unknown;
                p.unseen_finish = false;
            }
        }
        self.counts = self.compute_counts();
        self.desktop.badge(if self.badge { self.counts.finished as i64 } else { 0 });
    }

    pub fn command_of(&self, id: PaneId) -> &str {
        self.commands.get(&id).map(String::as_str).unwrap_or("")
    }

    fn resolve_pane(&self, explicit: Option<u32>, peer_pid: Option<u32>) -> Option<PaneId> {
        if let Some(id) = explicit.filter(|id| self.panes.contains_key(id)) {
            return Some(id);
        }
        let by_pid: HashMap<u32, PaneId> =
            self.panes.iter().filter_map(|(id, p)| Some((p.child_pid()?, *id))).collect();
        peer_pid
            .into_iter()
            .flat_map(procinfo::ancestry)
            .find_map(|pid| by_pid.get(&pid).copied())
    }

    fn set_agent_state(&mut self, id: PaneId, state: AgentState) {
        let focused_visible = self.ws().is_some_and(|w| w.focused() == Some(id));
        let ws_name = self
            .workspaces
            .iter()
            .find(|w| w.panes.contains(&id))
            .map(|w| format!("{}:{}", w.index, w.name))
            .unwrap_or_default();
        let Some(p) = self.panes.get_mut(&id) else { return };
        let was = p.agent_state;
        p.agent_state = state;
        if was == AgentState::Working && state == AgentState::Idle {
            p.unseen_finish = !focused_visible;
            if self.notifs {
                self.desktop.notify("rdreh", format!("Agent finished in {ws_name}"));
            }
        }
        self.refresh_tracking();
    }

    // ---------------------------------------------------------------- input

    pub fn handle_term_event(&mut self, ev: TermEvent) {
        match ev {
            TermEvent::Key(k) if k.kind != KeyEventKind::Release => self.handle_key(k),
            TermEvent::Mouse(m) => self.handle_mouse(m),
            TermEvent::Paste(text) => self.paste(&text),
            TermEvent::Resize(w, h) => self.resize(w, h),
            _ => {}
        }
    }

    fn paste(&mut self, text: &str) {
        if let Mode::Prompt { input, .. } = &mut self.mode {
            input.push_str(text.lines().next().unwrap_or(""));
            return;
        }
        if let Some(p) = self.focused_pane() {
            p.reset_scroll();
            let bracketed = p.parser.lock().unwrap().screen().bracketed_paste();
            if bracketed {
                p.write(format!("\x1b[200~{text}\x1b[201~").as_bytes());
            } else {
                p.write(text.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
            }
        }
    }

    fn forward_key(&mut self, key: KeyEvent) {
        if let Some(p) = self.focused_pane() {
            let app_cursor = p.parser.lock().unwrap().screen().application_cursor();
            if let Some(bytes) = keys::encode_key(key, app_cursor) {
                p.reset_scroll();
                p.write(&bytes);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match self.mode.clone() {
            Mode::Normal => self.normal_key(key),
            Mode::Prefix => {
                self.mode = Mode::Normal;
                self.prefix_key(key);
            }
            Mode::Help => self.mode = Mode::Normal,
            Mode::Confirm(kind) => {
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y' | 'Y')) {
                    match kind {
                        ConfirmKind::Quit => self.quit = true,
                        ConfirmKind::CloseWorkspace => self.close_workspace(),
                    }
                }
            }
            Mode::Prompt { kind, mut input } => match key.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    self.submit_prompt(kind, input.trim());
                }
                KeyCode::Backspace => {
                    input.pop();
                    self.mode = Mode::Prompt { kind, input };
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.push(c);
                    self.mode = Mode::Prompt { kind, input };
                }
                _ => self.mode = Mode::Prompt { kind, input },
            },
        }
    }

    fn normal_key(&mut self, key: KeyEvent) {
        let m = key.modifiers;
        let shift_only = m == KeyModifiers::SHIFT;
        let alt = m.contains(KeyModifiers::ALT) && !m.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('b') if m == KeyModifiers::CONTROL => self.mode = Mode::Prefix,
            KeyCode::Left if shift_only => self.focus_dir(Dir::Left),
            KeyCode::Right if shift_only => self.focus_dir(Dir::Right),
            KeyCode::Up if shift_only => self.focus_dir(Dir::Up),
            KeyCode::Down if shift_only => self.focus_dir(Dir::Down),
            KeyCode::PageUp if shift_only => self.scroll_focused(true),
            KeyCode::PageDown if shift_only => self.scroll_focused(false),
            KeyCode::Char(c) if alt => match c {
                '0'..='9' => self.switch_workspace(c as u8 - b'0'),
                '`' => self.cycle_pane(),
                'l' => self.cycle_layout(),
                'z' => self.toggle_zoom(),
                '=' | 'r' => self.reset_layout(),
                't' => self.toggle_tracked(),
                'f' => self.toggle_scope(),
                'n' => self.toggle_notifs(),
                _ => self.forward_key(key),
            },
            _ => self.forward_key(key),
        }
    }

    fn prefix_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('b') => self.forward_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            KeyCode::Char('h' | '?') => self.mode = Mode::Help,
            KeyCode::Char('c') => {
                let cwd = self.focused_cwd();
                self.add_project(&cwd, Some(Layout::Single));
            }
            KeyCode::Char('%' | '|' | '"' | '-' | 'n') => self.add_pane(),
            KeyCode::Char('x') => self.close_pane(),
            KeyCode::Char('&') => self.mode = Mode::Confirm(ConfirmKind::CloseWorkspace),
            KeyCode::Char(',') => {
                let input = self.ws().map(|w| w.name.clone()).unwrap_or_default();
                self.mode = Mode::Prompt { kind: PromptKind::Rename, input };
            }
            KeyCode::Char('a') => {
                let input = format!("{}/", self.focused_cwd().display());
                self.mode = Mode::Prompt { kind: PromptKind::AddProject, input };
            }
            KeyCode::Char('d') => self.detach_requested = true,
            KeyCode::Char('q') => self.mode = Mode::Confirm(ConfirmKind::Quit),
            KeyCode::Char('o') => self.cycle_pane(),
            KeyCode::Char('=') => self.reset_layout(),
            KeyCode::Char(' ') => self.cycle_layout(),
            KeyCode::Char('z') => self.toggle_zoom(),
            KeyCode::Char(c @ '0'..='9') => self.switch_workspace(c as u8 - b'0'),
            KeyCode::Left => self.focus_dir(Dir::Left),
            KeyCode::Right => self.focus_dir(Dir::Right),
            KeyCode::Up => self.focus_dir(Dir::Up),
            KeyCode::Down => self.focus_dir(Dir::Down),
            _ => {}
        }
    }

    fn submit_prompt(&mut self, kind: PromptKind, input: &str) {
        if input.is_empty() {
            return;
        }
        match kind {
            PromptKind::Rename => {
                if let Some(ws) = self.ws_mut() {
                    ws.name = input.to_string();
                }
            }
            PromptKind::AddProject => {
                if let Some(i) = self.add_project(&expand_tilde(input), None) {
                    self.flash(format!("Added workspace {i}"));
                }
            }
        }
    }

    fn scroll_focused(&mut self, up: bool) {
        if let Some(p) = self.focused_pane() {
            let rows = p.parser.lock().unwrap().screen().size().0.get() as isize;
            p.scroll(if up { rows / 2 } else { -rows / 2 });
        }
    }

    fn handle_mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        if let MouseEventKind::Down(MouseButton::Left) = m.kind
            && let Some((index, _)) = self.tab_hits.iter().find(|(_, r)| r.contains(pos))
        {
            self.switch_workspace(*index);
            return;
        }
        let Some((id, rect)) = self.visible.iter().find(|(_, r)| r.contains(pos)).copied() else {
            return;
        };
        if let MouseEventKind::Down(_) = m.kind
            && let Some(ws) = self.ws_mut()
            && let Some(i) = ws.panes.iter().position(|p| *p == id)
            && ws.focus != i
        {
            ws.focus = i;
            self.on_focus_changed();
            return; // focusing click is not forwarded
        }
        let Some(p) = self.panes.get(&id) else { return };
        let inner = inner(rect);
        let (mode, enc, alt_screen) = {
            let parser = p.parser.lock().unwrap();
            let s = parser.screen();
            (s.mouse_protocol_mode(), s.mouse_protocol_encoding(), s.alternate_screen())
        };
        if mode != vt100::MouseProtocolMode::None {
            if inner.contains(pos) {
                let (col, row) = (m.column - inner.x, m.row - inner.y);
                if let Some(bytes) = keys::encode_mouse(&m, col, row, mode, enc) {
                    p.write(&bytes);
                }
            }
            return;
        }
        let up = match m.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
        if alt_screen {
            // Alternate-scroll: wheel becomes arrow keys (less, man, git log).
            let seq: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
            p.write(&seq.repeat(3));
        } else {
            p.scroll(if up { 3 } else { -3 });
        }
    }

    // -------------------------------------------------------------- toggles

    fn toggle_tracked(&mut self) {
        let Some(ws) = self.ws_mut() else { return };
        ws.tracked = !ws.tracked;
        let msg = format!(
            "Workspace {} ({}): {} [Alt+t]",
            ws.index,
            ws.name,
            if ws.tracked { "INCLUDED in agent math" } else { "EXCLUDED from agent math" }
        );
        self.flash(msg);
        self.refresh_tracking();
    }

    fn toggle_scope(&mut self) {
        self.agents_only = !self.agents_only;
        self.flash(if self.agents_only {
            "Finish tracking: AGENTS WORKSPACE ONLY [Alt+f]"
        } else {
            "Finish tracking: ALL TRACKED WORKSPACES [Alt+f]"
        });
        self.refresh_tracking();
    }

    fn apply_toggle(cur: bool, action: &str) -> bool {
        match action {
            "on" | "enable" => true,
            "off" | "disable" | "mute" => false,
            _ => !cur,
        }
    }

    fn toggle_notifs(&mut self) {
        self.set_notifs("toggle");
    }

    fn set_notifs(&mut self, action: &str) -> String {
        self.notifs = Self::apply_toggle(self.notifs, action);
        let msg = format!("Desktop notifications: {}", if self.notifs { "ENABLED" } else { "MUTED" });
        self.flash(msg.clone());
        msg
    }

    fn set_badge(&mut self, action: &str) -> String {
        self.badge = Self::apply_toggle(self.badge, action);
        self.refresh_tracking();
        let msg = format!("Taskbar badge: {}", if self.badge { "ENABLED" } else { "MUTED" });
        self.flash(msg.clone());
        msg
    }

    // ------------------------------------------------------------------ IPC

    pub fn handle_ipc(&mut self, req: Request, peer_pid: Option<u32>) -> Response {
        match req {
            Request::State { state, pane } => {
                let st = match state.as_str() {
                    "working" | "busy" => AgentState::Working,
                    "idle" | "done" | "finished" => AgentState::Idle,
                    "reset" | "none" => AgentState::Unknown,
                    other => return Response::err(format!("unknown state {other:?}")),
                };
                match self.resolve_pane(pane, peer_pid) {
                    Some(id) => {
                        self.set_agent_state(id, st);
                        Response::ok(format!("pane {id}: {state}"))
                    }
                    None => Response::err("caller is not inside an rdr pane"),
                }
            }
            Request::List => Response::data(self.list_json()),
            Request::Counts => Response::data(json!({
                "total": self.counts.total,
                "busy": self.counts.busy,
                "finished": self.counts.finished,
                "title": self.window_title(),
            })),
            Request::SyncBadge { count } => {
                self.refresh_tracking();
                if let Some(c) = count {
                    self.desktop.badge(c);
                }
                Response::ok("badge synced")
            }
            Request::Notif { action } => Response::ok(self.set_notifs(&action)),
            Request::Badge { action } => Response::ok(self.set_badge(&action)),
            Request::Layout { preset } => match preset.as_str() {
                "cycle" | "next" => {
                    self.cycle_layout();
                    Response::ok("layout cycled")
                }
                "zoom" => {
                    self.toggle_zoom();
                    Response::ok("zoom toggled")
                }
                "reset" => {
                    self.reset_layout();
                    Response::ok("layout reset")
                }
                p => match Layout::parse(p) {
                    Some(l) => {
                        self.set_layout(l);
                        Response::ok(format!("layout {}", l.name()))
                    }
                    None => Response::err(format!("unknown layout {p:?}")),
                },
            },
            Request::Add { dir, layout } => {
                let layout = layout.as_deref().and_then(Layout::parse);
                match self.add_project(Path::new(&dir), layout) {
                    Some(i) => Response::ok(format!("added workspace {i}")),
                    None => Response::err(self.current_message().unwrap_or("add failed").to_string()),
                }
            }
            Request::Kill => {
                self.quit = true;
                Response::ok("shutting down")
            }
            Request::Attach { .. } => Response::err("attach is handled by the connection"),
        }
    }

    fn list_json(&self) -> serde_json::Value {
        let workspaces: Vec<_> = self
            .workspaces
            .iter()
            .enumerate()
            .map(|(pos, ws)| {
                let panes: Vec<_> = ws
                    .panes
                    .iter()
                    .enumerate()
                    .filter_map(|(i, id)| {
                        let p = self.panes.get(id)?;
                        Some(json!({
                            "id": id,
                            "focused": i == ws.focus,
                            "pid": p.child_pid(),
                            "command": self.command_of(*id),
                            "title": p.title.clone().or_else(|| p.osc_title()),
                            "agent": self.pane_is_agent(*id),
                            "state": p.agent_state,
                            "cwd": p.cwd(),
                        }))
                    })
                    .collect();
                json!({
                    "index": ws.index,
                    "name": ws.name,
                    "layout": ws.layout.name(),
                    "zoomed": ws.zoomed,
                    "tracked": ws.tracked,
                    "active": pos == self.active,
                    "panes": panes,
                })
            })
            .collect();
        json!({
            "session": self.cfg.general.session_name,
            "counts": { "total": self.counts.total, "busy": self.counts.busy, "finished": self.counts.finished },
            "notifications": self.notifs,
            "badge": self.badge,
            "finish_scope": if self.agents_only { "agents" } else { "all" },
            "workspaces": workspaces,
        })
    }
}
