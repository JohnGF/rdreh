# TODO

## Native Windows port (PowerShell / Windows Terminal)

Today rdreh is Unix-only and does not compile on Windows; WSL2 works as Linux.
A native port needs:

- [ ] **IPC:** Unix sockets → named pipes (`tokio::net::windows::named_pipe`),
      `\\.\pipe\rdr-<session>`; peer PID via `GetNamedPipeClientProcessId`.
- [ ] **PTY:** ConPTY (portable-pty already supports it); check the vt100
      callbacks still answer what PSReadLine and cmd ask for.
- [ ] **Process inspection** (`procinfo.rs` reads `/proc`): foreground program,
      parent chain and cwd via Toolhelp32 snapshots / `NtQueryInformationProcess`.
      Agent detection must match `claude.exe`, `node.exe` + script, etc.
- [ ] **Server detach:** no `setsid`/systemd scope → spawn with
      `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`.
- [ ] **Shells:** default to `pwsh` (fallback `powershell`, `cmd`); Ctrl+L
      clear-on-start for PSReadLine shells.
- [ ] **Agent hooks:** a PowerShell `swarm-agent-state.ps1` equivalent
      (`rdr state working|idle` itself is already portable).
- [ ] **Desktop:** toast notifications (WinRT) instead of D-Bus Notify;
      taskbar overlay badge via `ITaskbarList3::SetOverlayIcon`;
      `.lnk`/Start-menu entry instead of the `.desktop` file.
- [ ] **Launcher:** Windows Terminal (`wt.exe -w new`), conhost fallback.
- [ ] **Tokens:** replace the inode check (`MetadataExt::ino`) with the NTFS
      file index; paths under `%USERPROFILE%` / `%APPDATA%`.
- [ ] **CI:** `windows-latest` job running unit + e2e tests (the e2e harness
      uses Unix PTYs and needs a ConPTY variant).
- [ ] Gate Unix-only code behind `cfg(unix)` modules first, so the port lands
      in steps instead of one big change.

## macOS

- [ ] Replace `/proc` reads with `libproc` / `sysctl` (foreground program,
      ancestry, cwd).
- [ ] Data paths: OpenCode keeps `~/.local/share` on macOS, not
      `~/Library/Application Support`.
- [ ] Launcher: Terminal.app / iTerm2 / Ghostty via `open -na`.
- [ ] CI: `macos-latest` job.

## Desktops other than KDE (verify, not known broken)

- [ ] GNOME: confirm icon/grouping via `StartupWMClass=rdr`; badge only with
      Ubuntu Dock / Dash to Dock.
- [ ] Sway / Hyprland / i3, XFCE, Cinnamon: notifications via any
      freedesktop daemon (mako, dunst, ...).
