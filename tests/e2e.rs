//! End-to-end tests: run the real `rdr` binary inside a PTY, type at it, read
//! what it renders, and query it over its IPC socket.
//!
//! Each test gets its own temp dir used as `XDG_RUNTIME_DIR` (socket) and its
//! own session name, so tests run in parallel and never touch a live rdr.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_rdr");
const COLS: u16 = 160;
const ROWS: u16 = 40;

struct Harness {
    dir: tempfile::TempDir,
    session: String,
    env: Vec<(String, String)>,
    screen: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    _master: Box<dyn MasterPty + Send>,
}

/// One `rdr -t` client running in its own PTY.
struct Term {
    screen: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
}

fn spawn_client(dir: &Path, session: &str, env: &[(String, String)]) -> Term {
    let pair = native_pty_system()
        .openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
        .unwrap();
    let mut cmd = CommandBuilder::new(BIN);
    cmd.args(["-t", "--session", session, "--config"]);
    cmd.arg(dir.join("config.toml"));
    cmd.cwd(dir);
    cmd.env("XDG_RUNTIME_DIR", dir.join("run"));
    cmd.env("XDG_STATE_HOME", dir.join("state"));
    cmd.env("RDR_NO_SCOPE", "1");
    cmd.env("TERM", "xterm-256color");
    cmd.env("PS1", "$ ");
    cmd.env_remove("RDR_SOCKET");
    cmd.env_remove("RDR_PANE");
    cmd.env_remove("RDR_SESSION");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);

    let screen = Arc::new(Mutex::new(vt100::Parser::new(ROWS.try_into().unwrap(), COLS.try_into().unwrap(), 0)));
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    // crossterm probes the outer terminal (kitty-keyboard + DA1) at
    // startup; answer DA1 like a plain xterm so it doesn't wait for a timeout.
    let mut responder = pair.master.take_writer().map(Some).unwrap_or(None);
    {
        let screen = screen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let chunk = &buf[..n];
                if chunk.windows(3).any(|w| w == b"\x1b[c")
                    && let Some(w) = responder.as_mut()
                {
                    let _ = w.write_all(b"\x1b[?62;22c");
                }
                screen.lock().unwrap().process(chunk);
            }
        });
    }
    Term { screen, writer, child, master: pair.master }
}

impl Harness {
    fn start(session: &str, config: &str) -> Harness {
        Self::start_with(session, config, &[], &[])
    }

    /// `config`, `files` contents and `env` values may use `{dir}` for the
    /// per-test temp dir. `files` are written before rdr starts.
    fn start_with(session: &str, config: &str, files: &[(&str, &str)], env: &[(&str, &str)]) -> Harness {
        let dir = tempfile::Builder::new().prefix("rdr-e2e").tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("run")).unwrap();
        let sub = |v: &str| v.replace("{dir}", &dir.path().display().to_string());
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), sub(contents)).unwrap();
        }
        let config = config.replace("{dir}", &dir.path().display().to_string());
        // Tests may pick their own shell; otherwise /bin/sh (dash on Debian/Ubuntu).
        let shell = if config.contains("default_shell") { "" } else { "default_shell = \"/bin/sh\"\n" };
        std::fs::write(
            dir.path().join("config.toml"),
            format!("[general]\n{shell}taskbar_badge = false\n{config}"),
        )
        .unwrap();
        for (name, _) in files {
            if name.ends_with(".sh") || !name.contains('.') {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(dir.path().join(name), std::fs::Permissions::from_mode(0o755));
            }
        }
        let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), sub(v))).collect();
        let t = spawn_client(dir.path(), session, &env);
        let h = Harness {
            dir,
            session: session.into(),
            env,
            screen: t.screen,
            writer: t.writer,
            child: t.child,
            _master: t.master,
        };
        h.wait_until("socket to come up", |h| h.socket().exists() && h.try_list().is_some());
        h.wait_until("client attached and drawing", |h| h.screen_text().contains("Ctrl+b h help"));
        h
    }

    /// Attach a new terminal to the same server; returns the previous client.
    fn reattach(&mut self) -> Term {
        let t = spawn_client(self.dir.path(), &self.session, &self.env);
        let old = Term {
            screen: std::mem::replace(&mut self.screen, t.screen),
            writer: std::mem::replace(&mut self.writer, t.writer),
            child: std::mem::replace(&mut self.child, t.child),
            master: std::mem::replace(&mut self._master, t.master),
        };
        self.wait_until("new client drawing", |h| h.screen_text().contains("Ctrl+b h help"));
        old
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn socket(&self) -> PathBuf {
        self.path().join("run/rdr").join(format!("{}.sock", self.session))
    }

    fn client(&self) -> Command {
        let mut c = Command::new(BIN);
        c.env("XDG_RUNTIME_DIR", self.path().join("run"))
            .env_remove("RDR_SOCKET")
            .env_remove("RDR_PANE")
            .env_remove("RDR_SESSION")
            .args(["--session", &self.session]);
        c
    }

    fn rdr(&self, args: &[&str]) -> Output {
        self.client().args(args).output().unwrap()
    }

    fn rdr_ok(&self, args: &[&str]) -> String {
        let out = self.rdr(args);
        assert!(out.status.success(), "rdr {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn try_list(&self) -> Option<Value> {
        let out = self.rdr(&["list"]);
        out.status.success().then(|| serde_json::from_slice(&out.stdout).ok()).flatten()
    }

    fn list(&self) -> Value {
        self.try_list().expect("rdr list")
    }

    fn workspace(&self, name: &str) -> Value {
        self.list()["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("no workspace {name}"))
    }

    fn active(&self) -> Value {
        self.list()["workspaces"].as_array().unwrap().iter().find(|w| w["active"] == true).cloned().unwrap()
    }

    fn pane_ids(&self, ws: &str) -> Vec<u64> {
        self.workspace(ws)["panes"].as_array().unwrap().iter().map(|p| p["id"].as_u64().unwrap()).collect()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.writer.write_all(bytes).unwrap();
        self.writer.flush().unwrap();
        // Separate key events: crossterm reads ESC+x in one read as Alt+x.
        std::thread::sleep(Duration::from_millis(80));
    }

    fn screen_text(&self) -> String {
        self.screen.lock().unwrap().screen().contents()
    }

    fn wait_until(&self, what: &str, f: impl Fn(&Harness) -> bool) {
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(10) {
            if f(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}\n--- screen ---\n{}", self.screen_text());
    }

    fn wait_screen(&self, needle: &str) {
        self.wait_until(&format!("screen to contain {needle:?}"), |h| h.screen_text().contains(needle));
    }

    fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // The server outlives its client by design: stop it explicitly.
        let _ = self.rdr(&["kill"]);
        let _ = self.child.kill();
    }
}

const BASIC: &str = r#"
start_workspace = 1

[workspaces.1]
name = "agents"
layout = "quad"

[workspaces.2]
name = "editor"
layout = "single"
tracked = false
"#;

/// Command line for a process that rdreh sees as `claude` (its
/// /proc/<pid>/comm) but that really runs `sleep secs`. The symlink sets the
/// process name; `exec -a sleep` keeps argv[0] = "sleep", so multi-call
/// coreutils (which dispatch on argv[0]) still behave as sleep.
fn fake_agent_cmd(dir: &Path, secs: u32) -> String {
    // A script named `claude`: its process name is the file name, on any
    // distro (multi-call coreutils reject a renamed or relinked `sleep`).
    let script = dir.join("claude");
    if !script.exists() {
        std::fs::write(&script, "#!/bin/sh\nsleep \"$1\"\n").unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    format!("{} {secs}\r", script.display())
}

#[test]
fn boots_configured_workspaces_and_renders_status_bar() {
    let h = Harness::start("boot", BASIC);
    let l = h.list();
    let names: Vec<_> = l["workspaces"].as_array().unwrap().iter().map(|w| w["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["agents", "editor"]);
    assert_eq!(h.pane_ids("agents").len(), 4);
    assert_eq!(h.active()["name"], "agents");
    h.wait_screen("1:agents");
    h.wait_screen("Standby");
    h.wait_screen("Ctrl+b h help");
}

#[test]
fn panes_run_commands_and_receive_typed_input() {
    let mut h = Harness::start("typing", BASIC);
    h.send(b"echo hello-from-$((40+2))\r");
    h.wait_screen("hello-from-42");
}

#[test]
fn state_hook_drives_finished_counts_and_title() {
    let mut h = Harness::start("hook", BASIC);
    h.send(fake_agent_cmd(h.path(), 3011).as_bytes());
    h.wait_until("agent detected", |h| h.rdr_ok(&["counts"]) == "1 0 1");

    let id = h.pane_ids("agents")[0].to_string();
    let hook = |h: &Harness, state: &str| {
        let out = h.client().env("RDR_PANE", &id).args(["state", state]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty(), "hooks must be silent");
    };
    hook(&h, "working");
    assert_eq!(h.rdr_ok(&["counts"]), "1 1 0");
    h.wait_screen("● working");
    hook(&h, "idle");
    assert_eq!(h.rdr_ok(&["counts"]), "1 0 1");
    assert_eq!(h.rdr_ok(&["title"]), "[Finished: 1/1] rdreh");
    h.wait_screen("Finished: 1/1");
}

#[test]
fn state_hook_resolves_pane_from_process_ancestry() {
    let mut h = Harness::start("ancestry", BASIC);
    // Second pane, then report from inside it without RDR_PANE.
    h.send(b"\x1b[1;2C");
    h.send(b"env -u RDR_PANE \"$RDR_BIN\" state working\r");
    let second = h.pane_ids("agents")[1];
    h.wait_until("pane 2 working", |h| {
        h.workspace("agents")["panes"].as_array().unwrap().iter().any(|p| p["id"] == second && p["state"] == "working")
    });
}

#[test]
fn state_hook_outside_rdr_is_rejected() {
    let h = Harness::start("outside", BASIC);
    let out = h.rdr(&["state", "working"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not inside an rdr pane"));
}

#[test]
fn keyboard_drives_layout_focus_zoom_and_workspaces() {
    let mut h = Harness::start("keys", BASIC);
    h.send(b"\x1bl"); // Alt+l: quad -> single
    assert_eq!(h.active()["layout"], "single");
    h.send(b"\x1bl"); // single -> dual
    assert_eq!(h.active()["layout"], "dual");
    h.send(b"\x1bz"); // Alt+z
    assert_eq!(h.active()["zoomed"], true);
    h.send(b"\x1bz");
    h.send(b"\x1bl\x1bl"); // tri, quad (separate reads not needed for distinct events)
    h.wait_until("back to quad", |h| h.active()["layout"] == "quad");

    let focused = |h: &Harness| {
        h.active()["panes"].as_array().unwrap().iter().position(|p| p["focused"] == true).unwrap()
    };
    assert_eq!(focused(&h), 0);
    h.send(b"\x1b[1;2C"); // Shift+Right
    assert_eq!(focused(&h), 1);
    h.send(b"\x1b[1;2B"); // Shift+Down
    assert_eq!(focused(&h), 3);
    h.send(b"\x1b[1;2C"); // Shift+Right at the edge wraps to the next workspace
    assert_eq!(h.active()["name"], "editor");
    h.send(b"\x1b1"); // Alt+1
    assert_eq!(h.active()["name"], "agents");
}

#[test]
fn prefix_commands_add_and_close_panes_and_show_help() {
    let mut h = Harness::start("prefix", BASIC);
    h.send(b"\x02n"); // Ctrl+b n: add pane
    h.wait_until("5 panes", |h| h.pane_ids("agents").len() == 5);
    h.send(b"\x02x"); // Ctrl+b x: close focused pane
    h.wait_until("4 panes", |h| h.pane_ids("agents").len() == 4);
    h.send(b"\x02h");
    h.wait_screen("Press any key to close");
    h.send(b"q");
    h.wait_until("help closed", |h| !h.screen_text().contains("Press any key to close"));
    h.send(b"\x02,");
    h.send(b"\x7f\x7f\x7f\x7f\x7f\x7fbots\r");
    h.wait_until("renamed", |h| h.active()["name"] == "bots");
}

#[test]
fn ipc_layout_notif_and_badge_toggles() {
    let h = Harness::start("ipc", BASIC);
    h.rdr_ok(&["layout", "tri"]);
    assert_eq!(h.active()["layout"], "tri");
    assert!(h.rdr(&["layout", "bogus"]).status.code() != Some(0));
    assert_eq!(h.rdr_ok(&["notif", "on"]), "Desktop notifications: ENABLED");
    assert_eq!(h.list()["notifications"], true);
    assert_eq!(h.rdr_ok(&["badge", "off"]), "Taskbar badge: MUTED");
    assert_eq!(h.list()["badge"], false);
}

#[test]
fn second_launch_adds_dirs_or_refuses() {
    let h = Harness::start("single", BASIC);
    let proj = h.path().join("myproj");
    std::fs::create_dir(&proj).unwrap();

    // Attaching needs a terminal; without one, a bare `rdr -t` explains that.
    let refused = h.client().arg("-t").output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("needs a terminal"), "{}", String::from_utf8_lossy(&refused.stderr));

    let added = h.client().args(["-t", "-l", "dual"]).arg(&proj).output().unwrap();
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    let ws = h.workspace("myproj");
    assert_eq!(ws["layout"], "dual");
    assert_eq!(ws["panes"].as_array().unwrap().len(), 2);
    assert_eq!(h.active()["name"], "myproj");
}

#[test]
fn exited_panes_close_and_last_pane_quits() {
    let mut h = Harness::start(
        "exit",
        r#"
[workspaces.1]
name = "main"
[workspaces.2]
name = "short"
panes = [ { cmd = "sleep 0.5", hold = false } ]
"#,
    );
    h.wait_until("short-lived workspace removed", |h| {
        h.list()["workspaces"].as_array().unwrap().iter().all(|w| w["name"] != "short")
    });
    h.send(b"exit\r");
    h.wait_until("rdr exits with its last pane", |h| !h.socket().exists());
    let start = Instant::now();
    while !h.exited() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(h.exited());
}

#[test]
fn kill_shuts_down_cleanly() {
    let mut h = Harness::start("kill", BASIC);
    h.send(fake_agent_cmd(h.path(), 3017).as_bytes());
    h.wait_until("agent running", |h| h.rdr_ok(&["counts"]) == "1 0 1");
    let pid = h.workspace("agents")["panes"][0]["pid"].as_u64().unwrap();

    assert_eq!(h.rdr_ok(&["kill"]), "shutting down");
    h.wait_until("socket removed", |h| !h.socket().exists());
    let start = Instant::now();
    while !h.exited() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(h.exited(), "rdr did not exit");
    assert!(!Path::new(&format!("/proc/{pid}")).exists(), "pane shell survived");
    // The agent script's child runs exactly "sleep 3017".
    let survivors = Command::new("pgrep").args(["-fx", "sleep 3017"]).output().unwrap();
    assert!(survivors.stdout.is_empty(), "agent process survived shutdown");
}

// ------------------------------------------------------------ clear on start

const GREETING: &[(&str, &str)] = &[("greet.sh", "echo GREETING-XYZ\n")];
const GREETING_ENV: &[(&str, &str)] = &[("ENV", "{dir}/greet.sh")];

#[test]
fn fresh_shells_start_cleared() {
    // bash with a greeting rc file: has line editing, so Ctrl+L clears.
    let files = [
        ("greet.sh", "echo GREETING-XYZ\n"),
        ("bash-greet", "#!/bin/sh\nexec bash --noprofile --rcfile {dir}/greet.sh \"$@\"\n"),
    ];
    let cfg = format!("default_shell = \"{{dir}}/bash-greet\"\n{BASIC}");
    let h = Harness::start_with("clear", &cfg, &files, &[]);
    h.wait_until("greeting cleared and prompt redrawn", |h| {
        let t = h.screen_text();
        !t.contains("GREETING-XYZ") && t.contains("$ ")
    });
    let scrollback = h.screen.lock().unwrap().screen().contents();
    assert!(!scrollback.contains("GREETING-XYZ"));
}

#[test]
fn clear_on_start_can_be_disabled() {
    let cfg = format!("clear_on_start = false\n{BASIC}");
    let h = Harness::start_with("noclear", &cfg, GREETING, GREETING_ENV);
    h.wait_screen("GREETING-XYZ");
    std::thread::sleep(Duration::from_millis(1200));
    assert!(h.screen_text().contains("GREETING-XYZ"), "greeting must survive when disabled");
}

#[test]
fn startup_clear_never_interrupts_typing() {
    let mut h = Harness::start_with("typed", BASIC, GREETING, GREETING_ENV);
    h.send(b"echo typed-early\r");
    h.wait_screen("typed-early");
    std::thread::sleep(Duration::from_millis(1200));
    assert!(h.screen_text().contains("typed-early"), "typed output was cleared");
}

// ------------------------------------------------------- install / launcher

fn run(args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> Output {
    let mut c = Command::new(BIN);
    c.args(args).env_remove("RDR_SOCKET").env_remove("RDR_PANE").env_remove("RDR_SESSION");
    // Never touch the real token cache; tests that want one pass a path.
    c.env("RDR_TOKENS_CACHE", "off");
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

#[test]
fn install_writes_desktop_entry_and_icon_without_following_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("share");
    let apps = data.join("applications");
    std::fs::create_dir_all(&apps).unwrap();
    // A pre-existing symlink (e.g. into dotfiles) must be replaced, not written through.
    let foreign = dir.path().join("dotfiles-rdr.desktop");
    std::fs::write(&foreign, "ORIGINAL").unwrap();
    std::os::unix::fs::symlink(&foreign, apps.join("rdr.desktop")).unwrap();

    let env = [("XDG_DATA_HOME", data.as_os_str())];
    let out = run(&["install", "--no-refresh"], &env);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "ORIGINAL");
    let desktop_path = apps.join("rdr.desktop");
    assert!(!desktop_path.symlink_metadata().unwrap().file_type().is_symlink());
    let desktop = std::fs::read_to_string(&desktop_path).unwrap();
    let exe = std::fs::canonicalize(BIN).unwrap();
    assert!(desktop.contains(&format!("\nExec={}\n", exe.display())), "{desktop}");
    assert!(desktop.contains("\nIcon=rdr\n"));
    let icon = data.join("icons/hicolor/scalable/apps/rdr.svg");
    assert!(std::fs::read_to_string(&icon).unwrap().contains("<svg"));

    let out = run(&["uninstall", "--no-refresh"], &env);
    assert!(out.status.success());
    assert!(!desktop_path.exists() && !icon.exists());
    assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "ORIGINAL");
}

#[test]
fn completions_are_generated() {
    let out = run(&["completions", "fish"], &[]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("complete -c rdr"));
    assert!(text.contains("install"));
}

/// A fake terminal that records its argv instead of opening a window.
fn fake_terminal(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("argv.log");
    let term = dir.join("fake-term");
    std::fs::write(&term, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\n", log.display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&term, std::fs::Permissions::from_mode(0o755)).unwrap();
    (term, log)
}

fn wait_for_file(p: &Path) -> String {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(s) = std::fs::read_to_string(p)
            && !s.is_empty()
        {
            return s;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("{} never written", p.display());
}

#[test]
fn no_tty_launch_opens_configured_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let (term, log) = fake_terminal(dir.path());
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, format!("[general]\nterminal = \"{}\"\n", term.display())).unwrap();
    let run_dir = dir.path().join("run");
    // stdin is not a TTY here (Command::output), like KRunner.
    let out = run(
        &["--config", cfg.to_str().unwrap(), "--session", "launch", "-l", "tri"],
        &[("XDG_RUNTIME_DIR", run_dir.as_os_str())],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let argv: Vec<String> = wait_for_file(&log).lines().map(String::from).collect();
    let exe = std::env::current_dir().unwrap().join(BIN);
    assert_eq!(argv[0], "-e", "unknown terminals get -e: {argv:?}");
    assert_eq!(std::fs::canonicalize(&argv[1]).unwrap(), std::fs::canonicalize(exe).unwrap());
    assert_eq!(argv[2..], ["--terminal", "--config", cfg.to_str().unwrap(), "--session", "launch", "-l", "tri"]);
}

#[test]
fn term_flag_overrides_config_and_is_not_forwarded() {
    let dir = tempfile::tempdir().unwrap();
    let (term, log) = fake_terminal(dir.path());
    let cfg = dir.path().join("config.toml");
    std::fs::write(&cfg, "[general]\nterminal = \"/nonexistent/terminal\"\n").unwrap();
    let out = run(
        &["--config", cfg.to_str().unwrap(), "--term", term.to_str().unwrap()],
        &[("XDG_RUNTIME_DIR", dir.path().as_os_str())],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let argv: Vec<String> = wait_for_file(&log).lines().map(String::from).collect();
    assert!(!argv.iter().any(|a| a == "--term" || a == term.to_str().unwrap()), "{argv:?}");
    assert!(argv.iter().any(|a| a == "--terminal"), "{argv:?}");
}

// ---------------------------------------------------------------- tokens

fn pb_varint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// agy gen_metadata blob with usage at field path 1.4.
fn agy_blob(input: u64, output: u64, cached: u64) -> Vec<u8> {
    let field = |f: u64, wire: u64, out: &mut Vec<u8>| pb_varint((f << 3) | wire, out);
    let mut usage = Vec::new();
    for (f, v) in [(1, 1319), (2, input), (3, output), (5, cached)] {
        field(f, 0, &mut usage);
        pb_varint(v, &mut usage);
    }
    let mut inner = Vec::new();
    field(4, 2, &mut inner);
    pb_varint(usage.len() as u64, &mut inner);
    inner.extend(usage);
    let mut blob = Vec::new();
    field(1, 2, &mut blob);
    pb_varint(inner.len() as u64, &mut blob);
    blob.extend(inner);
    blob
}

#[test]
fn tokens_reads_every_tool_and_sorts_by_total() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let data = dir.path().join("data");

    // Claude: two distinct messages (one streamed twice), one old, one non-assistant line.
    let proj = home.join(".claude/projects/-tmp-x");
    std::fs::create_dir_all(&proj).unwrap();
    let now = chrono_free_now_rfc3339();
    let line = |id: &str, ts: &str, i: u64, o: u64, cr: u64| {
        format!(r#"{{"type":"assistant","timestamp":"{ts}","requestId":"r{id}","message":{{"id":"{id}","usage":{{"input_tokens":{i},"output_tokens":{o},"cache_read_input_tokens":{cr},"cache_creation_input_tokens":0}}}}}}"#)
    };
    std::fs::write(
        proj.join("s.jsonl"),
        [
            line("m1", &now, 100, 50, 1000),
            line("m1", &now, 100, 50, 1000),
            line("m0", "2020-01-01T00:00:00Z", 7, 3, 0),
            r#"{"type":"user","message":{"content":"hi"}}"#.to_string(),
            String::new(),
        ]
        .join("\n"),
    )
    .unwrap();

    // pi: one reply, repeated in a forked session file (same responseId), plus a user message.
    let pi = |sid: &str| {
        let d = home.join(".pi/agent/sessions/--tmp-x--");
        std::fs::create_dir_all(&d).unwrap();
        let reply = format!(r#"{{"type":"message","id":"a1","timestamp":"{now}","message":{{"role":"assistant","responseId":"resp-1","usage":{{"input":400,"output":60,"cacheRead":0,"cacheWrite":0,"reasoning":20,"totalTokens":460}}}}}}"#);
        let user = format!(r#"{{"type":"message","id":"u1","timestamp":"{now}","message":{{"role":"user","content":[{{"type":"text","text":"hi"}}]}}}}"#);
        std::fs::write(d.join(format!("{sid}.jsonl")), format!("{{\"type\":\"session\",\"id\":\"{sid}\"}}\n{user}\n{reply}\n")).unwrap();
    };
    pi("s1");
    pi("s1-fork");

    // OpenCode: one assistant message now, one user message.
    std::fs::create_dir_all(data.join("opencode")).unwrap();
    let oc = rusqlite::Connection::open(data.join("opencode/opencode.db")).unwrap();
    oc.execute_batch("CREATE TABLE message (id text primary key, session_id text, time_created integer, time_updated integer, data text);").unwrap();
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    oc.execute(
        "INSERT INTO message VALUES ('a','s',?1,?1,?2)",
        rusqlite::params![ms, r#"{"role":"assistant","tokens":{"input":20,"output":5,"reasoning":5,"cache":{"read":10,"write":2}}}"#],
    )
    .unwrap();
    oc.execute("INSERT INTO message VALUES ('u','s',?1,?1,'{\"role\":\"user\"}')", rusqlite::params![ms]).unwrap();
    drop(oc);

    // agy: one conversation with two generations plus a junk blob.
    let conv = home.join(".gemini/antigravity-cli/conversations");
    std::fs::create_dir_all(&conv).unwrap();
    let ag = rusqlite::Connection::open(conv.join("c1.db")).unwrap();
    ag.execute_batch("CREATE TABLE gen_metadata (idx integer primary key, data blob, size integer);").unwrap();
    for (i, blob) in [agy_blob(5000, 300, 20000), agy_blob(1000, 200, 0), vec![0xff, 0xff]].iter().enumerate() {
        ag.execute("INSERT INTO gen_metadata VALUES (?1, ?2, 0)", rusqlite::params![i as i64, blob]).unwrap();
    }
    drop(ag);

    let out = run(
        &["tokens", "--json"],
        &[("HOME", home.as_os_str()), ("XDG_DATA_HOME", data.as_os_str()), ("CLAUDE_CONFIG_DIR", std::ffi::OsStr::new(""))],
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["errors"], serde_json::json!([]));
    let window = |name: &str| v["windows"].as_array().unwrap().iter().find(|w| w["window"] == name).unwrap().clone();
    let totals = |w: &Value| -> Vec<(String, u64)> {
        w["tools"].as_array().unwrap().iter().map(|t| (t["tool"].as_str().unwrap().to_string(), t["total"].as_u64().unwrap())).collect()
    };
    // claude today: 100+50+1000 (m1 once); agy: 5300+20000 + 1200; pi: 460 once
    // (reasoning is inside output); opencode: 20+10+10+2.
    assert_eq!(
        totals(&window("today")),
        [("agy".into(), 26_500), ("claude".into(), 1_150), ("pi".into(), 460), ("opencode".into(), 42)]
    );
    // The 2020 claude message only shows up in all-time.
    let all = totals(&window("all_time"));
    assert!(all.contains(&("claude".into(), 1_160)), "{all:?}");
    let today_w = window("today");
    let claude_today = &today_w["tools"].as_array().unwrap().iter().find(|t| t["tool"] == "claude").unwrap()["usage"];
    assert_eq!(claude_today["calls"], 1, "streamed duplicate counted twice");
    let agy_today = today_w["tools"].as_array().unwrap().iter().find(|t| t["tool"] == "agy").unwrap();
    assert_eq!((agy_today["input"].as_u64(), agy_today["output"].as_u64()), (Some(26_000), Some(500)));
    assert!((agy_today["cache_hit"].as_f64().unwrap() - 20_000.0 / 26_000.0).abs() < 1e-9);

    // The dashboard renders all three tools, largest first.
    let text = String::from_utf8_lossy(&run(
        &["tokens"],
        &[("HOME", home.as_os_str()), ("XDG_DATA_HOME", data.as_os_str()), ("CLAUDE_CONFIG_DIR", std::ffi::OsStr::new(""))],
    ).stdout).to_string();
    let today = &text[text.find("TODAY").unwrap()..text.find("LAST 7 DAYS").unwrap()];
    let pos = |n: &str| today.find(n).unwrap_or_else(|| panic!("{n} missing:\n{today}"));
    assert!(pos("agy") < pos("claude") && pos("claude") < pos("opencode"), "{today}");
    // in = all input incl. cache: agy 5000+1000+20000 = 26.0k, out 300+200, hit 20000/26000.
    let strip = |l: &str| {
        let mut o = String::new();
        let mut esc = false;
        for c in l.chars() {
            match (esc, c) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (false, _) => o.push(c),
                _ => {}
            }
        }
        o
    };
    let agy_line = strip(today.lines().find(|l| l.contains("agy")).unwrap());
    let cols: Vec<&str> = agy_line.split_whitespace().filter(|w| !w.contains('█')).collect();
    // share: 26,500 of 28,152 (agy + claude + pi + opencode)
    assert_eq!(cols, ["agy", "26.5k", "94%", "26.0k", "500", "77%"], "{agy_line}");
    let claude_line = strip(today.lines().find(|l| l.contains("claude")).unwrap());
    let cols: Vec<&str> = claude_line.split_whitespace().filter(|w| !w.contains('█')).collect();
    assert_eq!(cols[3..], ["1.1k", "50", "91%"], "{claude_line}");
    let oc_line = strip(today.lines().find(|l| l.contains("opencode")).unwrap());
    let cols: Vec<&str> = oc_line.split_whitespace().collect();
    assert_eq!(cols[3..], ["32", "10", "31%"], "{oc_line}"); // in 20+10+2, out 5+5 reasoning
    let pi_line = today.lines().map(strip).find(|l| l.trim_start().starts_with("pi ")).unwrap();
    let cols: Vec<&str> = pi_line.split_whitespace().filter(|w| !w.contains('█')).collect();
    assert_eq!(cols, ["pi", "460", "2%", "400", "60", "0%"], "{pi_line}");
}

/// Current UTC time as RFC 3339 without pulling in a date crate.
fn chrono_free_now_rfc3339() -> String {
    let out = Command::new("date").args(["-u", "+%Y-%m-%dT%H:%M:%S.000Z"]).output().unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn tokens_handles_missing_sources() {
    let dir = tempfile::tempdir().unwrap();
    let out = run(
        &["tokens", "--json"],
        &[("HOME", dir.path().as_os_str()), ("XDG_DATA_HOME", dir.path().as_os_str()), ("CLAUDE_CONFIG_DIR", std::ffi::OsStr::new(""))],
    );
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["windows"][2]["tools"].as_array().unwrap().iter().all(|t| t["total"] == 0));
}

// ------------------------------------------------------------ layout cycle

#[test]
fn cycling_back_to_single_closes_unused_filler_panes_only() {
    let mut h = Harness::start("cycle", BASIC);
    h.send(b"\x1b2"); // Alt+2: editor, single, 1 pane
    assert_eq!(h.pane_ids("editor").len(), 1);
    h.send(b"\x1bl"); // dual: +1 pane
    h.send(b"\x1bl"); // tri: +1
    h.send(b"\x1bl"); // quad: +1
    h.wait_until("editor at quad", |h| h.active()["layout"] == "quad");
    assert_eq!(h.pane_ids("editor").len(), 4);

    // Use one filler: focus pane 2 (right) and type into it.
    h.send(b"\x1b[1;2C");
    let used = h.active()["panes"].as_array().unwrap().iter().find(|p| p["focused"] == true).unwrap()["id"].as_u64().unwrap();
    h.send(b"echo keep-me\r");
    h.wait_screen("keep-me");
    h.send(b"\x1b[1;2D"); // back to the original pane

    h.send(b"\x1bl"); // quad -> single
    h.wait_until("single", |h| h.active()["layout"] == "single");
    let ids = h.pane_ids("editor");
    assert_eq!(ids.len(), 2, "unused fillers closed, used one kept: {ids:?}");
    assert!(ids.contains(&used));
    h.wait_screen("single 1/2"); // hidden pane is visible in the status bar

    // Configured panes are never closed by cycling.
    h.send(b"\x1b1");
    h.send(b"\x1bl"); // agents quad -> single
    h.wait_until("agents single", |h| h.active()["layout"] == "single");
    assert_eq!(h.pane_ids("agents").len(), 4);
    h.wait_screen("single 1/4");
}

// ------------------------------------------------------- persistence (server)

fn wait_exit(child: &mut Box<dyn Child + Send + Sync>) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn term_text(t: &Term) -> String {
    t.screen.lock().unwrap().screen().contents()
}

#[test]
fn detach_keeps_everything_and_reattach_restores_it() {
    let mut h = Harness::start("persist", BASIC);
    h.send(b"echo persist-$((6*7))\r");
    h.wait_screen("persist-42");
    h.send(b"\x1bl"); // agents quad -> single: layout state must survive too
    h.wait_until("single", |h| h.active()["layout"] == "single");
    let panes_before = h.pane_ids("agents");

    h.send(b"\x02d"); // Ctrl+b d
    assert!(wait_exit(&mut h.child), "client should exit on detach");
    assert!(h.screen_text().contains("detached from rdreh"), "{}", h.screen_text());
    assert!(h.socket().exists() && h.try_list().is_some(), "server must keep running");
    assert_eq!(h.pane_ids("agents"), panes_before, "panes must survive a detach");

    let _old = h.reattach();
    h.wait_screen("persist-42"); // the shell's screen comes back
    assert_eq!(h.active()["layout"], "single");
    h.wait_screen("single 1/4");
}

#[test]
fn closing_the_terminal_does_not_end_the_session() {
    let mut h = Harness::start("closewin", BASIC);
    h.send(b"echo still-here\r");
    h.wait_screen("still-here");
    let _ = h.child.kill(); // the window goes away without a word
    assert!(wait_exit(&mut h.child));
    std::thread::sleep(Duration::from_millis(300));
    assert!(h.try_list().is_some(), "server died with its terminal");
    let _old = h.reattach();
    h.wait_screen("still-here");
}

#[test]
fn a_second_terminal_takes_over() {
    let mut h = Harness::start("takeover", BASIC);
    let mut old = h.reattach();
    assert!(wait_exit(&mut old.child), "first client should be detached");
    assert!(term_text(&old).contains("detached from rdreh"), "{}", term_text(&old));
    h.send(b"echo from-second\r");
    h.wait_screen("from-second");
}

#[test]
fn quit_ends_the_server_and_says_so() {
    let mut h = Harness::start("quit", BASIC);
    h.send(b"\x02q");
    h.wait_screen("Detach instead: Ctrl+b d");
    h.send(b"y");
    assert!(wait_exit(&mut h.child), "client should exit");
    assert!(h.screen_text().contains("rdreh exited"), "{}", h.screen_text());
    assert!(!h.socket().exists(), "server must be gone");
}

#[test]
fn attaching_to_an_old_version_explains_instead_of_garbling() {
    // Stand-in for a pre-server rdreh: answers any request with a JSON error.
    let dir = tempfile::Builder::new().prefix("rdr-e2e").tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("run/rdr")).unwrap();
    std::fs::write(dir.path().join("config.toml"), "[general]\ndefault_shell = \"/bin/sh\"\n").unwrap();
    let listener = std::os::unix::net::UnixListener::bind(dir.path().join("run/rdr/oldver.sock")).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut line = String::new();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let _ = std::io::BufRead::read_line(&mut reader, &mut line);
            let mut s = stream;
            let _ = s.write_all(b"{\"ok\":false,\"message\":\"bad request: unknown variant `attach`\"}\n");
        }
    });
    let mut t = spawn_client(dir.path(), "oldver", &[]);
    assert!(wait_exit(&mut t.child), "client should give up");
    let text = term_text(&t);
    assert!(text.contains("older version"), "{text}");
    assert!(!text.contains("\"ok\""), "raw JSON leaked to the screen: {text}");
}

#[test]
fn a_degenerate_start_size_never_creates_1x1_panes() {
    // Server started with no usable terminal size (as from a TTY-less script).
    let dir = tempfile::Builder::new().prefix("rdr-e2e").tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("run")).unwrap();
    let size_file = dir.path().join("size.txt");
    std::fs::write(
        dir.path().join("config.toml"),
        format!(
            "[general]\ndefault_shell = \"/bin/sh\"\ntaskbar_badge = false\n[workspaces.1]\nname = \"w\"\npanes = [ {{ cmd = \"stty size > {}\" }} ]\n",
            size_file.display()
        ),
    )
    .unwrap();
    let mut server = Command::new(BIN)
        .args(["--session", "zero", "--config"])
        .arg(dir.path().join("config.toml"))
        .args(["server", "--cols", "0", "--rows", "0"])
        .env("XDG_RUNTIME_DIR", dir.path().join("run"))
        .env("RDR_NO_SCOPE", "1")
        .env_remove("RDR_SOCKET")
        .env_remove("RDR_PANE")
        .stdin(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let text = wait_for_file(&size_file);
    let _ = Command::new(BIN)
        .args(["--session", "zero", "kill"])
        .env("XDG_RUNTIME_DIR", dir.path().join("run"))
        .env_remove("RDR_SOCKET")
        .output();
    let _ = server.wait();
    let dims: Vec<u16> = text.split_whitespace().map(|n| n.parse().unwrap()).collect();
    assert!(dims[0] >= 10 && dims[1] >= 40, "pane started at {dims:?}");
}

#[test]
fn shells_without_line_editing_are_never_sent_ctrl_l() {
    // Like dash: a shell with no line editor would type ^L into the next command.
    let files = [(
        "plainsh",
        "#!/bin/sh\n[ -x {dir}/plain ] || cp \"$(command -v bash)\" {dir}/plain\nexec {dir}/plain --noediting --noprofile --norc \"$@\"\n",
    )];
    let cfg = "default_shell = \"{dir}/plainsh\"\n[workspaces.1]\nname = \"w\"\n";
    let mut h = Harness::start_with("plain", cfg, &files, &[]);
    std::thread::sleep(Duration::from_millis(1200)); // past the startup-clear window
    h.send(b"echo plain-$((2+3))\r");
    h.wait_screen("plain-5");
    let text = h.screen_text();
    assert!(!text.contains("not found") && !text.contains("^L"), "{text}");
}

#[test]
fn copy_mode_navigation_selection_and_yank() {
    let mut h = Harness::start("copymode", BASIC);
    h.send(b"echo COPY_TEST_STRING_XYZ\r");
    h.wait_screen("COPY_TEST_STRING_XYZ");

    // Enter copy mode with Alt+v
    h.send(b"\x1bv");
    h.wait_screen("[COPY");

    // Move up to the output line, start selection, and yank
    h.send(b"k");
    h.send(b"v");
    h.wait_screen("[SELECT]");
    h.send(b"y");
    h.wait_screen("Copied");

    // Also test entering copy mode via Ctrl+b [ and exiting with q
    h.send(b"\x02[");
    h.wait_screen("[COPY");
    h.send(b"q");
    h.wait_until("exit copy mode", |h| !h.screen_text().contains("[COPY"));
}
