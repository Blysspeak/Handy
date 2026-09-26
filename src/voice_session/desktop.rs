use super::state::{lock, PasteRequest, Target};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type Reply = mpsc::SyncSender<Result<Value, String>>;

/// Which helper speaks the voice protocol for this session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Desktop {
    Kde,
    Hyprland,
}

/// From `XDG_CURRENT_DESKTOP`; KDE wins if a session names both.
fn detect(current_desktop: &str) -> Option<Desktop> {
    let current_desktop = current_desktop.to_lowercase();
    if current_desktop.contains("kde") {
        Some(Desktop::Kde)
    } else if current_desktop.contains("hyprland") {
        Some(Desktop::Hyprland)
    } else {
        None
    }
}

pub struct DesktopBridge {
    input: Mutex<ChildStdin>,
    child: Mutex<Child>,
    replies: Mutex<HashMap<u64, Reply>>,
    next_id: AtomicU64,
    directory: PathBuf,
    generation_write: Mutex<()>,
    /// One helper request at a time, so the reply deadline measures the
    /// helper's work instead of time spent queued behind a paste.
    request_serial: Mutex<()>,
    desktop: Desktop,
}

impl DesktopBridge {
    /// Hyprland shows voice state in the Omarchy bar, not a floating panel.
    pub fn bar_indicator(&self) -> bool {
        self.desktop == Desktop::Hyprland
    }

    pub fn start(
        on_event: impl Fn(String, Option<Target>) + Send + Sync + 'static,
    ) -> Result<Arc<Self>, String> {
        let desktop = detect(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default());
        let Some(desktop) = desktop
            .filter(|_| cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some())
        else {
            return Err("unsupported_desktop".into());
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| "desktop")?
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("diktor-voice-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).map_err(|_| "desktop")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| "desktop")?;
        }
        let mut script = directory.join("kde_bridge.py");
        std::fs::write(&script, include_str!("kde_bridge.py")).map_err(|_| "desktop")?;
        if desktop == Desktop::Hyprland {
            // Imports its guarded paste/Enter from kde_bridge.py next to it.
            script = directory.join("hyprland_bridge.py");
            std::fs::write(&script, include_str!("hyprland_bridge.py")).map_err(|_| "desktop")?;
        }
        std::fs::write(directory.join("generation"), "0").map_err(|_| "desktop")?;
        let mut child = Command::new("python3")
            .arg(&script)
            .arg(&directory)
            .arg(std::process::id().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|_| "desktop")?;
        let input = child.stdin.take().ok_or("desktop")?;
        let output = child.stdout.take().ok_or("desktop")?;
        let bridge = Arc::new(Self {
            input: Mutex::new(input),
            child: Mutex::new(child),
            replies: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            directory,
            generation_write: Mutex::new(()),
            request_serial: Mutex::new(()),
            desktop,
        });
        let weak = Arc::downgrade(&bridge);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else {
                    break;
                };
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(bridge) = weak.upgrade() else {
                    return;
                };
                if let Some(id) = value["id"].as_u64() {
                    if let Some(tx) = lock(&bridge.replies).remove(&id) {
                        let result = value["error"]
                            .as_str()
                            .map(|error| Err(error.to_string()))
                            .unwrap_or_else(|| Ok(value["result"].clone()));
                        let _ = tx.send(result);
                    }
                } else if let Some(event) = value["event"].as_str() {
                    if event == "ready" {
                        let _ = ready_tx.try_send(());
                    } else {
                        on_event(
                            event.to_string(),
                            serde_json::from_value(value["target"].clone()).ok(),
                        );
                    }
                }
            }
            if let Some(bridge) = weak.upgrade() {
                for (_, tx) in lock(&bridge.replies).drain() {
                    let _ = tx.try_send(Err("desktop".into()));
                }
                on_event("failed".into(), None);
            }
        });
        if ready_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            bridge.kill();
            return Err("desktop".into());
        }
        Ok(bridge)
    }

    /// File replacement is independent of helper stdin/D-Bus, including a hung
    /// helper. Every paste checks it again immediately before emitting keys.
    pub fn set_generation(&self, generation: u64) -> Result<(), String> {
        let _write = lock(&self.generation_write);
        let temporary = self.directory.join("generation-next");
        std::fs::write(&temporary, generation.to_string()).map_err(|_| "desktop")?;
        std::fs::rename(temporary, self.directory.join("generation"))
            .map_err(|_| "desktop".to_string())
    }

    pub fn request(self: &Arc<Self>, mut value: Value) -> Result<Value, String> {
        let _serial = lock(&self.request_serial);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        value["id"] = json!(id);
        let (tx, rx) = mpsc::sync_channel(1);
        {
            let mut replies = lock(&self.replies);
            if replies.len() >= 8 {
                return Err("desktop".into());
            }
            replies.insert(id, tx);
        }
        let bridge = Arc::clone(self);
        // A blocked pipe must not defeat the request deadline/watchdog.
        std::thread::spawn(move || {
            let mut input = lock(&bridge.input);
            if writeln!(input, "{value}")
                .and_then(|_| input.flush())
                .is_err()
            {
                if let Some(tx) = lock(&bridge.replies).remove(&id) {
                    let _ = tx.send(Err("desktop".into()));
                }
            }
        });
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(result) => result,
            Err(_) => {
                lock(&self.replies).remove(&id);
                self.kill();
                Err("desktop".into())
            }
        }
    }

    pub fn enable(self: &Arc<Self>, generation: u64) -> Result<Target, String> {
        self.set_generation(generation)?;
        let result = self.request(json!({"op": "enable", "generation": generation}))?;
        serde_json::from_value(result).map_err(|_| "target_unavailable".into())
    }
    pub fn target(self: &Arc<Self>) -> Result<Target, String> {
        serde_json::from_value(self.request(json!({"op":"target"}))?)
            .map_err(|_| "target_unavailable".into())
    }
    pub fn paste(self: &Arc<Self>, request: &PasteRequest) -> Result<(), String> {
        self.request(json!({"op":"paste", "generation":request.generation, "target":request.target, "text":request.text}))
            .map(|_| ())
            .map_err(paste_error)
    }
    /// One Enter for a pasted batch; the helper rechecks generation and window.
    pub fn submit(self: &Arc<Self>, generation: u64, target: &Target) -> Result<(), String> {
        self.request(submit_message(generation, target))
            .map(|_| ())
            .map_err(paste_error)
    }
    pub fn disable(self: &Arc<Self>) {
        if self.set_generation(0).is_err() {
            self.kill();
        }
        let _ = self.request(json!({"op":"disable"}));
    }
    pub fn kill(&self) {
        let mut child = lock(&self.child);
        let _ = child.kill();
        let _ = child.wait();
    }
    pub fn shutdown(self: &Arc<Self>) {
        self.disable();
        let _ = self.request(json!({"op":"shutdown"}));
        self.kill();
    }
}

fn submit_message(generation: u64, target: &Target) -> Value {
    json!({"op": "submit", "generation": generation, "target": target})
}

fn paste_error(error: String) -> String {
    if error == "desktop" {
        // A lost reply/pipe/timeout may follow successful key injection.
        "delivery_uncertain".into()
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn voice_paste_transport_failure_cannot_be_retried() {
        assert_eq!(super::paste_error("desktop".into()), "delivery_uncertain");
        assert_eq!(
            super::paste_error("target_changed".into()),
            "target_changed"
        );
    }

    #[test]
    fn voice_bridge_follows_current_desktop_and_kde_keeps_its_helper() {
        use super::{detect, Desktop};
        assert_eq!(detect("KDE"), Some(Desktop::Kde));
        assert_eq!(detect("Hyprland"), Some(Desktop::Hyprland));
        assert_eq!(detect("Hyprland:KDE"), Some(Desktop::Kde));
        assert_eq!(detect("GNOME"), None);
        assert_eq!(detect(""), None);
    }

    #[test]
    fn voice_submit_request_carries_only_generation_and_window() {
        let target = super::Target {
            id: "window-A".into(),
            app_id: "com.mitchellh.ghostty".into(),
            pid: 71,
            output: "DP-1".into(),
            output_x: 0,
            output_y: 0,
        };
        let message = super::submit_message(4, &target);
        assert_eq!(message["op"], "submit");
        assert_eq!(message["generation"], 4);
        assert_eq!(message["target"]["id"], "window-A");
        assert!(message.get("text").is_none());
    }
}
