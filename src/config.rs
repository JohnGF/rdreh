//! `~/.config/rdr/config.toml`: everything user- or machine-specific lives here
//! so the binary itself carries no dotfile coupling.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ratatui::style::Color;
use serde::{Deserialize, Serialize};

use crate::layout::Layout;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub general: General,
    pub theme: Theme,
    /// Keyed by workspace number ("0".."9"), as in `[workspaces.1]`.
    pub workspaces: BTreeMap<String, WorkspaceConfig>,
    /// `rdr tokens` sources: `[[tokens.jsonl]]` adds a harness.
    pub tokens: crate::tokens::TokensConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    pub default_shell: String,
    /// Names the IPC socket; run two independent swarms with different names.
    pub session_name: String,
    /// Desktop notification when a tracked agent finishes (Alt+n toggles).
    pub notification_popups: bool,
    /// KDE/Unity taskbar badge with the finished-agent count.
    pub taskbar_badge: bool,
    /// `all`: every tracked workspace counts. `agents`: only the workspace named "agents".
    pub finish_scope: String,
    /// Layout used for workspaces created from `rdr <dir>` / Ctrl+b a.
    pub default_layout: Layout,
    pub scrollback: usize,
    /// Foreground process names (prefix match) treated as AI agents.
    pub agent_commands: Vec<String>,
    /// Desktop-file URIs that receive the LauncherEntry badge.
    pub badge_app_uris: Vec<String>,
    /// `desktop-entry` hint attached to notifications.
    pub desktop_entry: String,
    /// Workspace focused at startup (defaults to the lowest-numbered one).
    pub start_workspace: Option<u8>,
    /// Terminal for new windows (KRunner/app-menu launches, --term). May carry
    /// flags, e.g. "kitty -1". Unset: $TERMINAL, then the first installed known one.
    pub terminal: Option<String>,
    /// Clear plain shell panes once their startup output (greetings, fastfetch) settles.
    pub clear_on_start: bool,
    /// Run the window's terminal from a copy of its binary named `rdr`, so KDE
    /// Plasma shows rdr as its own app instead of grouping it with the terminal.
    pub taskbar_identity: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            default_shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()),
            session_name: "rdr".into(),
            notification_popups: false,
            taskbar_badge: true,
            finish_scope: "all".into(),
            default_layout: Layout::Quad,
            scrollback: 10_000,
            agent_commands: [
                "agy", "oc", "opencode", "claude", "aider", "ai", "pi", "gemini", "codex",
            ]
            .map(String::from)
            .to_vec(),
            badge_app_uris: vec!["application://rdr.desktop".into()],
            desktop_entry: "rdr".into(),
            start_workspace: None,
            terminal: None,
            clear_on_start: true,
            taskbar_identity: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub border_active: String,
    pub border_inactive: String,
    pub status_bg: String,
    pub status_fg: String,
    pub working: String,
    pub finished: String,
}

impl Default for Theme {
    fn default() -> Self {
        // "radar": the palette of the rdr icon (slate, emerald, amber, sky).
        Self {
            border_active: "#10B981".into(),
            border_inactive: "#334155".into(),
            status_bg: "#0F141C".into(),
            status_fg: "#CBD5E1".into(),
            working: "#F59E0B".into(),
            finished: "#38BDF8".into(),
        }
    }
}

pub fn parse_color(s: &str) -> Color {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() == 6
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        return Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
    }
    s.parse().unwrap_or(Color::Reset)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub name: Option<String>,
    pub layout: Option<Layout>,
    pub cwd: Option<String>,
    /// Explicit panes. When empty, `count` panes run `cmd` (or the shell).
    pub panes: Vec<PaneConfig>,
    pub count: Option<usize>,
    pub cmd: Option<String>,
    /// Whether agents in this workspace count toward Finished n/m.
    pub tracked: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaneConfig {
    pub cmd: Option<String>,
    pub title: Option<String>,
    pub cwd: Option<String>,
    /// Drop into the shell when `cmd` exits instead of closing the pane.
    pub hold: bool,
}

impl Default for PaneConfig {
    fn default() -> Self {
        Self { cmd: None, title: None, cwd: None, hold: true }
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if p == "~" {
        return dirs::home_dir().unwrap_or_default();
    }
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(p)
}

pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("rdr/config.toml")
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Config> {
        let path = path.map(Path::to_path_buf).unwrap_or_else(default_path);
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str::<Config>(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Config::default()
        };
        if cfg.workspaces.is_empty() {
            cfg.workspaces = Self::builtin_workspaces();
        }
        for key in cfg.workspaces.keys() {
            key.parse::<u8>()
                .ok()
                .filter(|n| *n <= 9)
                .with_context(|| format!("workspace key {key:?} must be a number 0-9"))?;
        }
        Ok(cfg)
    }

    /// Used when the config file defines no workspaces.
    fn builtin_workspaces() -> BTreeMap<String, WorkspaceConfig> {
        let mut m = BTreeMap::new();
        m.insert(
            "1".into(),
            WorkspaceConfig {
                name: Some("agents".into()),
                layout: Some(Layout::Quad),
                ..Default::default()
            },
        );
        m.insert(
            "2".into(),
            WorkspaceConfig {
                name: Some("editor".into()),
                layout: Some(Layout::Single),
                tracked: Some(false),
                ..Default::default()
            },
        );
        m
    }

    pub fn agents_only(&self) -> bool {
        matches!(self.general.finish_scope.as_str(), "agents" | "agents-only")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_example_parses() {
        let text = r##"
[general]
default_shell = "/usr/bin/fish"
session_name = "agent-swarm"
notification_popups = false
taskbar_badge = true

[theme]
border_active = "#88C0D0"

[workspaces.0]
name = "telemetry"
layout = "cockpit"
tracked = false
panes = [
  { cmd = "laya-tool-tracker", title = "Tool Tracker" },
  { cmd = "fish", title = "Control Shell", hold = false },
]

[workspaces.1]
name = "agents"
layout = "quad"
count = 4
"##;
        let cfg: Config = toml::from_str(text).unwrap();
        assert_eq!(cfg.general.session_name, "agent-swarm");
        assert_eq!(cfg.workspaces["0"].layout, Some(Layout::Cockpit));
        assert_eq!(cfg.workspaces["0"].panes.len(), 2);
        assert!(cfg.workspaces["0"].panes[0].hold);
        assert!(!cfg.workspaces["0"].panes[1].hold);
        assert_eq!(cfg.workspaces["1"].count, Some(4));
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#88C0D0"), Color::Rgb(0x88, 0xC0, 0xD0));
        assert_eq!(parse_color("red"), Color::Red);
    }
}
