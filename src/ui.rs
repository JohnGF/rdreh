//! Rendering: vt100 screens -> ratatui buffer, pane borders with agent state,
//! bottom status bar and modal overlays.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use crate::app::{App, ConfirmKind, Mode, PromptKind, inner};
use crate::config::parse_color;
use crate::pane::{AgentState, Pane};

pub const HELP: &str = "\
 rdreh (rdr): keyboard-first agent multiplexer

 No prefix
   Alt+0..9          jump to workspace
   Shift+Arrows      move between panes (left/right wrap workspaces)
   Alt+`             cycle panes
   Alt+l             cycle layout: single > dual > tri > quad > rows > cockpit
   Alt+z             zoom focused pane
   Alt+v             copy / view mode (scroll, select, yank)
   Alt+= / Alt+r     reset layout (unzoom)
   Alt+t             include/exclude workspace from agent math
   Alt+f             finish scope: all workspaces / agents only
   Alt+n             desktop notifications on/off
   Shift+PgUp/PgDn   scrollback (any key returns to live)
   Shift+Enter       newline in agent prompts (sends Meta+Enter)
   Mouse             click to focus, drag to copy, wheel to scroll
   Ctrl+click        open URL or file (relative to the pane's cwd)

 Ctrl+b, then
   h ?               this help          a   add project folder
   c                 new workspace      n % | \" -   add pane
   x                 close pane         &   close workspace
   [ v               copy / view mode   ,   rename workspace
   Space             cycle layout       =   reset layout
   0..9              workspace          d   detach (keeps running)
   p                 paste clipboard (copied files become paths)
   q                 quit rdreh (ends every pane)
   Ctrl+b            send a literal Ctrl+b to the pane

 Hooks: rdr state working|idle   (inside any pane)
 Press any key to close.";

struct Theme {
    active: Color,
    inactive: Color,
    bar_bg: Color,
    bar_fg: Color,
    working: Color,
    finished: Color,
}

fn vt_color(c: vt100::Color) -> Color {
    match c {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

#[derive(Clone, Copy)]
pub struct SelectionRange {
    pub start_abs_row: usize,
    pub start_col: u16,
    pub end_abs_row: usize,
    pub end_col: u16,
}

fn render_screen(screen: &vt100::Screen, area: Rect, buf: &mut Buffer, selection: Option<SelectionRange>) {
    let total_history = screen.scrollback_len();
    let top_abs = total_history.saturating_sub(screen.scrollback());
    for row in 0..area.height {
        let abs_row = top_abs + row as usize;
        for col in 0..area.width {
            let Some(cell) = screen.cell(row, col) else { continue };
            if cell.is_wide_continuation() {
                continue;
            }
            let Some(out) = buf.cell_mut((area.x + col, area.y + row)) else { continue };
            let mut mods = Modifier::empty();
            if cell.bold() {
                mods |= Modifier::BOLD;
            }
            if cell.dim() {
                mods |= Modifier::DIM;
            }
            if cell.italic() {
                mods |= Modifier::ITALIC;
            }
            if cell.underline() {
                mods |= Modifier::UNDERLINED;
            }
            if cell.inverse() {
                mods |= Modifier::REVERSED;
            }

            if let Some(sel) = selection {
                let is_selected = if sel.start_abs_row == sel.end_abs_row {
                    abs_row == sel.start_abs_row && col >= sel.start_col && col <= sel.end_col
                } else if abs_row == sel.start_abs_row {
                    col >= sel.start_col
                } else if abs_row == sel.end_abs_row {
                    col <= sel.end_col
                } else {
                    abs_row > sel.start_abs_row && abs_row < sel.end_abs_row
                };
                if is_selected {
                    mods |= Modifier::REVERSED;
                }
            }

            let contents = cell.contents();
            out.set_symbol(if contents.is_empty() { " " } else { contents });
            out.set_style(
                Style::default()
                    .fg(vt_color(cell.fgcolor()))
                    .bg(vt_color(cell.bgcolor()))
                    .add_modifier(mods),
            );
        }
    }
}

fn pane_label(app: &App, pane: &Pane) -> String {
    pane.title
        .clone()
        .or_else(|| pane.osc_title())
        .unwrap_or_else(|| app.command_of(pane.id).to_string())
}

/// Returns the cursor style the focused pane asked for.
pub fn draw(f: &mut Frame, app: &mut App) -> Option<vt100::CursorStyle> {
    let theme = Theme {
        active: parse_color(&app.cfg.theme.border_active),
        inactive: parse_color(&app.cfg.theme.border_inactive),
        bar_bg: parse_color(&app.cfg.theme.status_bg),
        bar_fg: parse_color(&app.cfg.theme.status_fg),
        working: parse_color(&app.cfg.theme.working),
        finished: parse_color(&app.cfg.theme.finished),
    };
    let area = f.area();
    let mut cursor_style = None;

    let (visible, focused) = match app.ws() {
        Some(ws) => (app.visible_panes(ws), ws.focused()),
        None => (Vec::new(), None),
    };

    for (n, (id, rect)) in visible.iter().enumerate() {
        let Some(pane) = app.panes.get(id) else { continue };
        let is_focused = Some(*id) == focused;
        let border = if is_focused { theme.active } else { theme.inactive };

        let mut title = vec![Span::raw(format!(" {} {} ", n + 1, pane_label(app, pane)))];
        let copy_state = match &app.mode {
            Mode::Copy(s) if s.pane_id == *id => Some(s),
            _ => None,
        };
        let parser = pane.parser.lock().unwrap();
        let screen = parser.screen();
        if copy_state.is_some() {
            title.push(Span::styled("[COPY] ", Style::new().fg(theme.working).add_modifier(Modifier::BOLD)));
        }
        if screen.scrollback() > 0 {
            title.push(Span::styled(format!("[↑{}] ", screen.scrollback()), Style::new().fg(theme.working)));
        }
        let status = match pane.agent_state {
            AgentState::Working => Some(Span::styled(" ● working ", Style::new().fg(theme.working))),
            AgentState::Idle if pane.unseen_finish => Some(Span::styled(
                " ✓ done ",
                Style::new().fg(theme.bar_bg).bg(theme.finished).add_modifier(Modifier::BOLD),
            )),
            AgentState::Idle => Some(Span::styled(" ✓ done ", Style::new().fg(theme.finished))),
            AgentState::Unknown => None,
        };
        let mut block = Block::bordered()
            .border_type(if is_focused { BorderType::Thick } else { BorderType::Rounded })
            .border_style(Style::new().fg(border))
            .title(Line::from(title).style(Style::new().fg(border).add_modifier(if is_focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            })));
        if let Some(s) = status {
            block = block.title(Line::from(s).right_aligned());
        }
        f.render_widget(block, *rect);
        let inner_rect = inner(*rect);

        let selection = copy_state.and_then(|s| {
            s.selection_anchor.map(|(a_row, a_col)| {
                let (sr, sc, er, ec) = if a_row < s.cursor_abs_row
                    || (a_row == s.cursor_abs_row && a_col <= s.cursor_col)
                {
                    (a_row, a_col, s.cursor_abs_row, s.cursor_col)
                } else {
                    (s.cursor_abs_row, s.cursor_col, a_row, a_col)
                };
                let cols = screen.size().1.get();
                if s.line_wise {
                    SelectionRange { start_abs_row: sr, start_col: 0, end_abs_row: er, end_col: cols.saturating_sub(1) }
                } else {
                    SelectionRange { start_abs_row: sr, start_col: sc, end_abs_row: er, end_col: ec }
                }
            })
        });

        render_screen(screen, inner_rect, f.buffer_mut(), selection);

        if is_focused {
            if let Some(s) = copy_state {
                let top_abs = screen.scrollback_len().saturating_sub(screen.scrollback());
                if s.cursor_abs_row >= top_abs && s.cursor_abs_row < top_abs + inner_rect.height as usize {
                    let vis_row = (s.cursor_abs_row - top_abs) as u16;
                    let vis_col = s.cursor_col.min(inner_rect.width.saturating_sub(1));
                    f.set_cursor_position((inner_rect.x + vis_col, inner_rect.y + vis_row));
                    cursor_style = Some(vt100::CursorStyle::SteadyBlock);
                }
            } else if app.mode == Mode::Normal && screen.scrollback() == 0 && !screen.hide_cursor() {
                let (row, col) = screen.cursor_position();
                if row < inner_rect.height && col < inner_rect.width {
                    f.set_cursor_position((inner_rect.x + col, inner_rect.y + row));
                    cursor_style = Some(screen.cursor_style());
                }
            }
        }
    }
    app.visible = visible;

    draw_status(f, app, &theme, Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1));
    draw_overlay(f, app, &theme, area);
    cursor_style
}

fn draw_status(f: &mut Frame, app: &mut App, theme: &Theme, bar: Rect) {
    let base = Style::new().bg(theme.bar_bg).fg(theme.bar_fg);
    let mut spans = vec![Span::styled(
        " rdreh ",
        Style::new().bg(theme.active).fg(theme.bar_bg).add_modifier(Modifier::BOLD),
    )];
    spans.push(Span::raw(" "));
    let mut x = bar.x + 8;
    app.tab_hits.clear();
    for (pos, ws) in app.workspaces.iter().enumerate() {
        let busy = ws.panes.iter().any(|id| app.panes.get(id).is_some_and(|p| p.agent_state == AgentState::Working));
        let unseen = ws.panes.iter().any(|id| app.panes.get(id).is_some_and(|p| p.unseen_finish));
        let marker = if unseen { "✓" } else if busy { "●" } else { "" };
        let zoom = if ws.zoomed { "Z" } else { "" };
        let label = format!(" {}:{}{}{} ", ws.index, ws.name, marker, zoom);
        let width = label.chars().count() as u16;
        let style = if pos == app.active {
            Style::new().bg(theme.active).fg(theme.bar_bg).add_modifier(Modifier::BOLD)
        } else if unseen {
            Style::new().fg(theme.finished).add_modifier(Modifier::BOLD)
        } else if busy {
            Style::new().fg(theme.working)
        } else {
            base
        };
        app.tab_hits.push((ws.index, Rect::new(x, bar.y, width, 1)));
        x += width + 1;
        spans.push(Span::styled(label, style));
        spans.push(Span::raw(" "));
    }

    if let Some(ws) = app.ws() {
        // Single/zoom with hidden panes: say so, e.g. "single 2/4".
        let label = if ws.monocle() && ws.panes.len() > 1 {
            let name = if ws.zoomed { "zoom" } else { ws.layout.name() };
            format!("│ {name} {}/{} ", ws.focus + 1, ws.panes.len())
        } else {
            format!("│ {} ", ws.layout.name())
        };
        spans.push(Span::styled(label, Style::new().fg(theme.inactive)));
    }
    let c = app.counts;
    spans.push(if c.total == 0 {
        Span::styled("│ Standby ", Style::new().fg(theme.inactive))
    } else {
        Span::styled(
            format!("│ Finished: {}/{} ", c.finished, c.total),
            Style::new().fg(theme.finished).add_modifier(Modifier::BOLD),
        )
    });
    if app.agents_only {
        spans.push(Span::styled("[agents only] ", Style::new().fg(theme.inactive)));
    }
    spans.push(Span::styled(
        if app.notifs { "│ notif on " } else { "│ notif off " },
        Style::new().fg(theme.inactive),
    ));
    match &app.mode {
        Mode::Prefix => spans.push(Span::styled(" PREFIX ", Style::new().bg(theme.working).fg(theme.bar_bg))),
        Mode::Copy(state) => {
            let sel = if state.selection_anchor.is_some() {
                if state.line_wise { " [LINE SELECT] " } else { " [SELECT] " }
            } else {
                " [COPY MODE] "
            };
            spans.push(Span::styled(sel, Style::new().bg(theme.working).fg(theme.bar_bg).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled(" y: copy, v: select, V: line, q: exit ", Style::new().fg(theme.bar_fg)));
        }
        _ => {
            if let Some(m) = app.current_message() {
                spans.push(Span::styled(format!("│ {m}"), Style::new().fg(theme.working)));
            }
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(base), bar);
    let hint = " Ctrl+b h help ";
    let hw = hint.len() as u16;
    if bar.width > hw + 40 {
        f.render_widget(
            Paragraph::new(hint).style(Style::new().fg(theme.inactive).bg(theme.bar_bg)),
            Rect::new(bar.right() - hw, bar.y, hw, 1),
        );
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn draw_overlay(f: &mut Frame, app: &App, theme: &Theme, area: Rect) {
    let block = |title: &str| {
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(theme.active))
            .title(format!(" {title} "))
            .style(Style::new().bg(theme.bar_bg).fg(theme.bar_fg))
    };
    match &app.mode {
        Mode::Help => {
            let lines = HELP.lines().count() as u16;
            let r = centered(area, 72, lines + 2);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(HELP).block(block("keys")), r);
        }
        Mode::Prompt { kind, input } => {
            let title = match kind {
                PromptKind::AddProject => "add project folder",
                PromptKind::Rename => "rename workspace",
            };
            let r = centered(area, 70, 3);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(format!("{input}█")).block(block(title)), r);
        }
        Mode::Confirm(kind) => {
            let q = match kind {
                ConfirmKind::Quit => "Quit rdreh and end every pane? (y/n)  Detach instead: Ctrl+b d",
                ConfirmKind::CloseWorkspace => "Close this workspace and its panes? (y/n)",
            };
            let r = centered(area, 64, 4);
            f.render_widget(Clear, r);
            f.render_widget(Paragraph::new(q).wrap(Wrap { trim: true }).block(block("confirm")), r);
        }
        Mode::Normal | Mode::Prefix | Mode::Copy(_) => {}
    }
}
