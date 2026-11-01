//! Unix-socket JSON-lines IPC. Agent hooks call `rdr state working|idle`,
//! which connects here; the server answers from the UI event loop.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tokio::sync::{mpsc::UnboundedSender, oneshot};

use crate::app::Event;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    /// Mark a pane working/idle. `pane` comes from `$RDR_PANE`; without it the
    /// server resolves the pane from the caller's process ancestry.
    State { state: String, pane: Option<u32> },
    List,
    Counts,
    SyncBadge { count: Option<i64> },
    Notif { action: String },
    Badge { action: String },
    Layout { preset: String },
    Add { dir: String, layout: Option<String> },
    Kill,
    /// Turn this connection into an attached terminal: the client then sends
    /// crossterm events as JSON lines and receives rendered frames.
    Attach { cols: u16, rows: u16 },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(msg: impl Into<String>) -> Self {
        Self { ok: true, message: Some(msg.into()), data: None }
    }
    pub fn data(v: serde_json::Value) -> Self {
        Self { ok: true, message: None, data: Some(v) }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, message: Some(msg.into()), data: None }
    }
}

pub fn socket_path(session: &str) -> PathBuf {
    if let Ok(p) = std::env::var("RDR_SOCKET")
        && !p.is_empty()
    {
        return PathBuf::from(p);
    }
    let base = dirs::runtime_dir()
        .unwrap_or_else(|| std::env::temp_dir().join(format!("rdr-{}", unsafe { libc::getuid() })));
    base.join("rdr").join(format!("{session}.sock"))
}

/// Bind the socket, refusing if another live instance owns it.
pub fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    if path.exists() {
        if StdUnixStream::connect(path).is_ok() {
            bail!("another rdr instance is already running on {}", path.display());
        }
        std::fs::remove_file(path)?; // stale socket from a crash
    }
    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}

pub fn serve(listener: UnixListener, events: UnboundedSender<Event>) {
    let next_client = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let events = events.clone();
            let next_client = next_client.clone();
            tokio::spawn(async move {
                let peer_pid = stream.peer_cred().ok().and_then(|c| c.pid()).map(|p| p as u32);
                let (rd, mut wr) = stream.into_split();
                let mut lines = tokio::io::BufReader::new(rd).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Ok(Request::Attach { cols, rows }) = serde_json::from_str::<Request>(&line) {
                        let id = next_client.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let (out, mut frames) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
                        // Frames -> socket. Ends when the server drops `out`.
                        tokio::spawn(async move {
                            while let Some(bytes) = frames.recv().await {
                                if wr.write_all(&bytes).await.is_err() {
                                    break;
                                }
                            }
                            let _ = wr.shutdown().await;
                        });
                        if events.send(Event::Attach { id, cols, rows, out }).is_err() {
                            return;
                        }
                        // Socket -> input events, until the client goes away.
                        while let Ok(Some(line)) = lines.next_line().await {
                            if let Ok(ev) = serde_json::from_str::<crossterm::event::Event>(&line) {
                                let _ = events.send(Event::Input { id, ev });
                            }
                        }
                        let _ = events.send(Event::Detached { id });
                        return;
                    }
                    let resp = match serde_json::from_str::<Request>(&line) {
                        Ok(req) => {
                            let (tx, rx) = oneshot::channel();
                            if events.send(Event::Ipc(req, peer_pid, tx)).is_err() {
                                return;
                            }
                            rx.await.unwrap_or_else(|_| Response::err("rdr is shutting down"))
                        }
                        Err(e) => Response::err(format!("bad request: {e}")),
                    };
                    let mut out = serde_json::to_vec(&resp).unwrap_or_default();
                    out.push(b'\n');
                    if wr.write_all(&out).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

pub fn request(path: &Path, req: &Request) -> Result<Response> {
    let mut stream = StdUnixStream::connect(path)
        .with_context(|| format!("rdr is not running (no socket at {})", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let mut resp = String::new();
    BufReader::new(stream).read_line(&mut resp)?;
    Ok(serde_json::from_str(&resp)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format() {
        let j = serde_json::to_string(&Request::State { state: "working".into(), pane: Some(3) }).unwrap();
        assert_eq!(j, r#"{"cmd":"state","state":"working","pane":3}"#);
        let r: Request = serde_json::from_str(r#"{"cmd":"sync-badge","count":2}"#).unwrap();
        assert!(matches!(r, Request::SyncBadge { count: Some(2) }));
    }
}
