//! Native D-Bus desktop integration: Unity LauncherEntry taskbar badge (KDE
//! Plasma honours it) and freedesktop notifications. Runs on its own task so a
//! slow or missing session bus never blocks the UI.

use std::collections::HashMap;

use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use zbus::Variant as Value;

pub enum DesktopCmd {
    Badge(i64),
    Notify { summary: String, body: String },
}

pub struct Desktop {
    tx: UnboundedSender<DesktopCmd>,
    task: tokio::task::JoinHandle<()>,
}

impl Desktop {
    pub fn start(app_uris: Vec<String>, desktop_entry: String) -> Desktop {
        let (tx, mut rx) = unbounded_channel::<DesktopCmd>();
        let task = tokio::spawn(async move {
            let Ok(conn) = zbus::Connection::session().await else {
                while rx.recv().await.is_some() {} // no bus: drain silently
                return;
            };
            let mut last_badge = None;
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    DesktopCmd::Badge(count) => {
                        if last_badge == Some(count) {
                            continue;
                        }
                        last_badge = Some(count);
                        let mut props: HashMap<&str, Value> = HashMap::new();
                        props.insert("count", Value::I64(count));
                        props.insert("count-visible", Value::Bool(count > 0));
                        for uri in &app_uris {
                            let _ = conn
                                .emit_signal(
                                    None::<&str>,
                                    "/com/canonical/unity/launcherentry/rdr",
                                    "com.canonical.Unity.LauncherEntry",
                                    "Update",
                                    &(uri.as_str(), &props),
                                )
                                .await;
                        }
                    }
                    DesktopCmd::Notify { summary, body } => {
                        let mut hints: HashMap<&str, Value> = HashMap::new();
                        hints.insert("desktop-entry", Value::from(desktop_entry.as_str()));
                        let _ = conn
                            .call_method(
                                Some("org.freedesktop.Notifications"),
                                "/org/freedesktop/Notifications",
                                Some("org.freedesktop.Notifications"),
                                "Notify",
                                &(
                                    "rdreh",
                                    0u32,
                                    "rdr",
                                    summary.as_str(),
                                    body.as_str(),
                                    Vec::<&str>::new(),
                                    &hints,
                                    -1i32,
                                ),
                            )
                            .await;
                    }
                }
            }
        });
        Desktop { tx, task }
    }

    pub fn badge(&self, count: i64) {
        let _ = self.tx.send(DesktopCmd::Badge(count));
    }

    pub fn notify(&self, summary: impl Into<String>, body: impl Into<String>) {
        let _ = self.tx.send(DesktopCmd::Notify { summary: summary.into(), body: body.into() });
    }

    /// Clear the badge and let pending D-Bus messages flush before exit.
    pub async fn shutdown(self) {
        let _ = self.tx.send(DesktopCmd::Badge(0));
        drop(self.tx);
        let _ = tokio::time::timeout(std::time::Duration::from_millis(500), self.task).await;
    }
}
