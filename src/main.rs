mod app;
mod client;
mod config;
mod desktop;
mod install;
mod ipc;
mod keys;
mod launcher;
mod layout;
mod pane;
mod procinfo;
mod server;
mod tokens;
mod ui;

use std::ffi::OsString;
use std::io::{IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand, ValueHint};
use crossterm::terminal;

use crate::config::Config;
use crate::ipc::Request;

#[derive(Parser)]
#[command(
    name = "rdr",
    version,
    about = "rdreh: keyboard-first terminal multiplexer for AI coding agents"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Project folders to open as extra workspaces (sent to the running
    /// instance if one exists).
    #[arg(value_hint = ValueHint::DirPath)]
    dirs: Vec<PathBuf>,
    /// Layout for project workspaces: single, dual, tri, quad, rows.
    #[arg(short, long)]
    layout: Option<String>,
    /// Config file (default: ~/.config/rdr/config.toml).
    #[arg(long, global = true, value_hint = ValueHint::FilePath)]
    config: Option<PathBuf>,
    /// Session name (selects the socket); overrides config and $RDR_SESSION.
    #[arg(long, global = true)]
    session: Option<String>,
    /// Open in a new window of this terminal (e.g. kitty, ghostty, alacritty,
    /// "kitty -1"). Default when started without a TTY: general.terminal,
    /// then $TERMINAL, then the first installed known terminal.
    #[arg(long = "term", value_name = "TERMINAL")]
    term: Option<String>,
    /// Shortcut for --term alacritty.
    #[arg(long, conflicts_with_all = ["konsole", "term"])]
    alacritty: bool,
    /// Shortcut for --term konsole.
    #[arg(long, conflicts_with = "term")]
    konsole: bool,
    /// Always run in the current terminal (never spawn a window).
    #[arg(short = 't', long)]
    terminal: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Report agent state for the calling pane (used by agent hooks).
    State { state: String },
    /// Print JSON describing every workspace and pane.
    List,
    /// Print "total busy finished" agent counts.
    Counts,
    /// Print the window title ("[Finished: n/m] rdreh").
    Title,
    /// Re-sync the taskbar badge, optionally forcing a count.
    SyncBadge { count: Option<i64> },
    /// Desktop notifications: on | off | toggle.
    #[command(alias = "popups")]
    Notif { action: Option<String> },
    /// Taskbar badge: on | off | toggle.
    Badge { action: Option<String> },
    /// Change the active workspace layout (single, dual, tri, quad, rows, cockpit, cycle, zoom, reset).
    Layout { preset: String },
    /// Add a project folder as a new workspace.
    Add {
        #[arg(value_hint = ValueHint::DirPath)]
        dir: PathBuf,
        layout: Option<String>,
    },
    /// Shut down the running instance.
    Kill,
    /// Print the key cheat-sheet.
    Keys,
    /// Print an example config file.
    DefaultConfig,
    /// Install the desktop entry and icon (KRunner, app menu, taskbar).
    Install {
        /// Skip refreshing desktop/icon caches.
        #[arg(long)]
        no_refresh: bool,
    },
    /// Remove what `rdr install` put in place.
    Uninstall {
        #[arg(long)]
        no_refresh: bool,
    },
    /// Token usage per agent tool (claude, opencode, agy) for today, 7 days, all time.
    Tokens {
        /// Redraw continuously (for a cockpit pane).
        #[arg(long)]
        watch: bool,
        /// Seconds between refreshes in --watch mode.
        #[arg(long, default_value_t = 10)]
        interval: u64,
        /// Print JSON instead of the dashboard.
        #[arg(long, conflicts_with = "watch")]
        json: bool,
    },
    /// Run the rdreh server in the foreground (normally started by `rdr`).
    #[command(hide = true)]
    Server {
        #[arg(long, default_value_t = 160)]
        cols: u16,
        #[arg(long, default_value_t = 48)]
        rows: u16,
        #[arg(long)]
        layout: Option<String>,
        dirs: Vec<PathBuf>,
    },
    /// Print shell completions (bash, zsh, fish, elvish, powershell).
    Completions { shell: clap_complete::Shell },
}

fn session_name(cli: &Cli, cfg: &Config) -> String {
    cli.session
        .clone()
        .or_else(|| std::env::var("RDR_SESSION").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| cfg.general.session_name.clone())
}

fn client(cli: &Cli, cfg: &Config, cmd: &Cmd) -> Result<()> {
    let socket = ipc::socket_path(&session_name(cli, cfg));
    let action = |a: &Option<String>| a.clone().unwrap_or_else(|| "toggle".into());
    let req = match cmd {
        Cmd::State { state } => Request::State {
            state: state.clone(),
            pane: std::env::var("RDR_PANE").ok().and_then(|p| p.parse().ok()),
        },
        Cmd::List => Request::List,
        Cmd::Counts | Cmd::Title => Request::Counts,
        Cmd::SyncBadge { count } => Request::SyncBadge { count: *count },
        Cmd::Notif { action: a } => Request::Notif { action: action(a) },
        Cmd::Badge { action: a } => Request::Badge { action: action(a) },
        Cmd::Layout { preset } => Request::Layout { preset: preset.clone() },
        Cmd::Add { dir, layout } => Request::Add {
            dir: std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone()).display().to_string(),
            layout: layout.clone(),
        },
        Cmd::Kill => Request::Kill,
        Cmd::Keys | Cmd::DefaultConfig | Cmd::Install { .. }
        | Cmd::Uninstall { .. }
        | Cmd::Completions { .. }
        | Cmd::Tokens { .. }
        | Cmd::Server { .. } => {
            unreachable!()
        }
    };
    let resp = ipc::request(&socket, &req)?;
    if !resp.ok {
        bail!(resp.message.unwrap_or_else(|| "request failed".into()));
    }
    match (cmd, resp.data) {
        (Cmd::Counts, Some(d)) => println!("{} {} {}", d["total"], d["busy"], d["finished"]),
        (Cmd::Title, Some(d)) => println!("{}", d["title"].as_str().unwrap_or("")),
        (_, Some(d)) => println!("{}", serde_json::to_string_pretty(&d)?),
        (Cmd::State { .. }, None) => {} // hooks want silence
        (_, None) => {
            if let Some(m) = resp.message {
                println!("{m}");
            }
        }
    }
    Ok(())
}

/// Re-exec ourselves inside a new terminal window, detached.
fn launch_window(cli: &Cli, cfg: &Config) -> Result<()> {
    let flag = cli.term.clone().or_else(|| {
        if cli.konsole {
            Some("konsole".into())
        } else if cli.alacritty {
            Some("alacritty".into())
        } else {
            None
        }
    });
    let env_terminal = std::env::var("TERMINAL").ok();
    let base = launcher::resolve(
        flag.as_deref(),
        cfg.general.terminal.as_deref(),
        env_terminal.as_deref(),
        launcher::on_path,
    )
    .context("no terminal found: set general.terminal in the config, $TERMINAL, or pass --term")?;

    // Our own arguments minus the window-selection ones.
    let mut inner = vec![std::env::current_exe()?.display().to_string(), "--terminal".into()];
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--alacritty" | "--konsole" => {}
            "--term" => {
                args.next();
            }
            _ if a.starts_with("--term=") => {}
            _ => inner.push(a),
        }
    }

    let argv = launcher::argv(&base, &inner);
    let mut program = PathBuf::from(&argv[0]);
    if cfg.general.taskbar_identity
        && let Some(src) = launcher::find_in_path(&argv[0])
        && let Some(data) = dirs::data_dir()
    {
        match launcher::identity_binary(&src, &data) {
            Ok(Some(copy)) => program = copy,
            Ok(None) => {}
            Err(e) => eprintln!("rdr: taskbar identity unavailable ({e}); using {}", argv[0]),
        }
    }
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&argv[1..]);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: setsid is async-signal-safe; detaches the window from our session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().with_context(|| format!("launching {} ({})", argv[0], program.display()))?;
    Ok(())
}

fn parse_layout(l: Option<&str>) -> Result<Option<layout::Layout>> {
    l.map(|l| layout::Layout::parse(l).with_context(|| format!("unknown layout {l:?}"))).transpose()
}

fn is_live(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Start the server detached from this terminal and wait for its socket.
/// It gets its own systemd scope, so closing the window that started it
/// (or the KRunner service it ran in) does not take it down.
fn start_server(cli: &Cli, session: &str, socket: &Path) -> Result<()> {
    // Panes are created at this size; a missing or degenerate terminal (0x0)
    // would start them at 1x1, which some programs (watch) never recover from.
    let (cols, rows) = terminal::size().ok().filter(|(c, r)| *c >= 20 && *r >= 5).unwrap_or((160, 48));
    let exe = std::env::current_exe()?;
    let mut args: Vec<OsString> = vec!["--session".into(), session.into()];
    if let Some(c) = &cli.config {
        args.push("--config".into());
        args.push(std::fs::canonicalize(c).unwrap_or_else(|_| c.clone()).into());
    }
    args.extend(["server", "--cols", &cols.to_string(), "--rows", &rows.to_string()].map(OsString::from));
    if let Some(l) = &cli.layout {
        args.extend(["--layout".into(), l.into()]);
    }
    for d in &cli.dirs {
        args.push(std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()).into());
    }

    let log_dir = dirs::state_dir().unwrap_or_else(std::env::temp_dir).join("rdr");
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("{session}.log"));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;

    let runtime = dirs::runtime_dir().unwrap_or_default();
    let use_scope = std::env::var_os("RDR_NO_SCOPE").is_none()
        && launcher::on_path("systemd-run")
        && runtime.join("systemd/private").exists();
    let mut cmd = if use_scope {
        let unit: String = session.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        let mut c = std::process::Command::new("systemd-run");
        c.args(["--user", "--scope", "--quiet", "--collect"])
            .arg(format!("--unit=rdreh-{unit}-{}", std::process::id()))
            .arg("--")
            .arg(&exe);
        c
    } else {
        std::process::Command::new(&exe)
    };
    cmd.args(&args)
        .env_remove("RDR_PANE")
        .env_remove("RDR_SOCKET")
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; the server leaves our session.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting the rdreh server")?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if is_live(socket) {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            let tail = std::fs::read_to_string(&log_path).unwrap_or_default();
            let tail: Vec<&str> = tail.lines().rev().take(8).collect();
            bail!(
                "rdreh server exited ({status}). Log {}:\n{}",
                log_path.display(),
                tail.into_iter().rev().collect::<Vec<_>>().join("\n")
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    bail!("rdreh server did not come up; see {}", log_path.display())
}

fn add_dirs(cli: &Cli, socket: &Path) -> Result<()> {
    for d in &cli.dirs {
        let dir = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
        let req = Request::Add { dir: dir.display().to_string(), layout: cli.layout.clone() };
        let r = ipc::request(socket, &req)?;
        if !r.ok {
            bail!(r.message.unwrap_or_else(|| "add failed".into()));
        }
        println!("{}", r.message.unwrap_or_default());
    }
    Ok(())
}

/// "2 working · 1 done / 4 agents" from the running swarm, if reachable.
fn swarm_line(cli: &Cli, cfg: &Config) -> Option<String> {
    let resp = ipc::request(&ipc::socket_path(&session_name(cli, cfg)), &Request::Counts).ok()?;
    let d = resp.data?;
    let (total, busy, done) = (d["total"].as_u64()?, d["busy"].as_u64()?, d["finished"].as_u64()?);
    Some(if total == 0 {
        "swarm: standby".into()
    } else {
        format!("swarm: {busy} working · {done} done / {total}")
    })
}

fn run_tokens(cli: &Cli, cfg: &Config, watch: bool, interval: u64, json: bool) -> Result<()> {
    let mut scanner = tokens::Scanner::with_cache(tokens::Paths::detect(), tokens::default_cache_path());
    if watch {
        // The first scan reads every transcript (seconds on a large history).
        print!("\x1b[?25l\x1b[H\x1b[2J \x1b[1;36mrdreh · tokens\x1b[0m  \x1b[2mscanning agent history…\x1b[0m");
        std::io::stdout().flush()?;
    }
    scanner.refresh();
    if json {
        println!("{}", serde_json::to_string_pretty(&tokens::summary_json(&scanner, tokens::now_ms()))?);
        return Ok(());
    }
    let width = || terminal::size().map(|(w, _)| usize::from(w)).unwrap_or(80);
    if !watch {
        print!("{}", tokens::render(&scanner, tokens::now_ms(), width(), swarm_line(cli, cfg).as_deref()));
        return Ok(());
    }
    let mut out = std::io::stdout();
    loop {
        let frame = tokens::render(&scanner, tokens::now_ms(), width(), swarm_line(cli, cfg).as_deref());
        write!(out, "\x1b[?25l\x1b[H\x1b[2J{frame}")?;
        out.flush()?;
        std::thread::sleep(Duration::from_secs(interval.max(1)));
        scanner.refresh();
    }
}

const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(cli.config.as_deref())?;

    match &cli.cmd {
        Some(Cmd::Keys) => {
            println!("{}", ui::HELP);
            return Ok(());
        }
        Some(Cmd::DefaultConfig) => {
            print!("{EXAMPLE_CONFIG}");
            return Ok(());
        }
        Some(Cmd::Install { no_refresh }) => {
            for p in install::install(!no_refresh)? {
                println!("installed {}", p.display());
            }
            return Ok(());
        }
        Some(Cmd::Uninstall { no_refresh }) => {
            for p in install::uninstall(!no_refresh)? {
                println!("removed {}", p.display());
            }
            return Ok(());
        }
        Some(Cmd::Tokens { watch, interval, json }) => {
            return run_tokens(&cli, &cfg, *watch, *interval, *json);
        }
        Some(Cmd::Server { cols, rows, layout, dirs }) => {
            let args = server::ServerArgs {
                cols: *cols,
                rows: *rows,
                dirs: dirs.clone(),
                layout: parse_layout(layout.as_deref())?,
            };
            let session = session_name(&cli, &cfg);
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
            return rt.block_on(server::run(cfg, session, args));
        }
        Some(Cmd::Completions { shell }) => {
            clap_complete::generate(*shell, &mut Cli::command(), "rdr", &mut std::io::stdout());
            return Ok(());
        }
        Some(cmd) => return client(&cli, &cfg, cmd),
        None => {}
    }

    let session = session_name(&cli, &cfg);
    let socket = ipc::socket_path(&session);

    // Inside an rdreh pane: never nest. Folders go to the running server.
    if let Ok(pane) = std::env::var("RDR_PANE") {
        if !cli.dirs.is_empty() && is_live(&socket) {
            return add_dirs(&cli, &socket);
        }
        bail!("already inside rdreh (pane {pane}). Use Ctrl+b c for a new workspace, or `rdr <dir>` to add a project.");
    }

    // Started from KRunner/.desktop (no TTY) or asked explicitly: open a
    // window running `rdr --terminal`, which attaches (or starts) the server.
    let want_window = cli.term.is_some()
        || cli.alacritty
        || cli.konsole
        || (!cli.terminal && !std::io::stdin().is_terminal());
    if want_window {
        return launch_window(&cli, &cfg);
    }

    if is_live(&socket) {
        add_dirs(&cli, &socket)?;
    } else {
        start_server(&cli, &session, &socket)?;
    }
    if !std::io::stdin().is_terminal() {
        if cli.dirs.is_empty() {
            bail!("attaching needs a terminal: run `rdr` from one (the server is running)");
        }
        return Ok(()); // e.g. `rdr -t <dir>` from a script: folders added, nothing to attach
    }

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    match rt.block_on(client::attach(&socket))? {
        client::End::Detached => println!("[detached from rdreh: run `rdr` to reattach]"),
        client::End::Exited => println!("[rdreh exited]"),
        client::End::Lost => {}
    }
    Ok(())
}
