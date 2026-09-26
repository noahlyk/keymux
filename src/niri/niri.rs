use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::Sender;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info};

use crate::window_manager::{WindowInfo, WindowManager, WindowManagerEvent};

#[derive(Clone)]
pub struct Niri;

impl Niri {
    pub const fn new() -> Self {
        Self
    }

    fn detect_niri_socket() -> Option<PathBuf> {
        if let Ok(socket_path) = std::env::var("NIRI_SOCKET") {
            let path = PathBuf::from(&socket_path);
            if path.exists() {
                info!("Using NIRI_SOCKET from env: {}", socket_path);
                return Some(path);
            }
            tracing::warn!(
                "NIRI_SOCKET env var set but file doesn't exist: {}",
                socket_path
            );
        }

        let uid = unsafe { libc::getuid() };
        let runtime_dir = format!("/run/user/{uid}");

        if let Ok(entries) = std::fs::read_dir(&runtime_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(filename) = path.file_name() {
                    if filename.to_string_lossy().starts_with("niri.") && path.exists() {
                        info!("Found Niri socket via scan: {}", path.display());
                        return Some(path);
                    }
                }
            }
        }

        None
    }

    fn get_focused_window_info() -> WindowInfo {
        let Ok(output) = Command::new("niri")
            .args(["msg", "focused-window"])
            .output()
        else {
            return WindowInfo {
                app_id: None,
                pid: None,
                title: None,
            };
        };

        if !output.status.success() {
            return WindowInfo {
                app_id: None,
                pid: None,
                title: None,
            };
        }

        let Ok(text) = String::from_utf8(output.stdout) else {
            return WindowInfo {
                app_id: None,
                pid: None,
                title: None,
            };
        };

        let mut app_id = None;
        let mut pid = None;
        let mut title = None;

        for line in text.lines() {
            let trimmed = line.trim();

            if let Some(app_id_part) = trimmed.strip_prefix("App ID:") {
                let id = app_id_part.trim().trim_matches('"');
                app_id = Some(id.to_string());
            } else if let Some(pid_part) = trimmed.strip_prefix("PID:") {
                if let Ok(pid_num) = pid_part.trim().parse::<u32>() {
                    pid = Some(pid_num);
                }
            } else if let Some(title_part) = trimmed.strip_prefix("Title:") {
                let t = title_part.trim().trim_matches('"');
                title = Some(t.to_string());
            }
        }

        WindowInfo { app_id, pid, title }
    }
}

impl Default for Niri {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowManager for Niri {
    fn name(&self) -> &'static str {
        "niri"
    }

    fn is_available(&self) -> bool {
        Self::detect_niri_socket().is_some()
    }

    fn detect_socket(&self) -> Option<PathBuf> {
        Self::detect_niri_socket()
    }

    fn get_focused_window(&self) -> WindowInfo {
        Self::get_focused_window_info()
    }

    fn event_stream_args(&self) -> Vec<&'static str> {
        vec!["msg", "event-stream"]
    }

    fn parse_event(&self, line: &str) -> Option<WindowInfo> {
        if line.starts_with("Window focus changed:") {
            Some(Self::get_focused_window_info())
        } else {
            None
        }
    }
}

pub fn is_niri_available() -> bool {
    Niri::new().is_available()
}

pub fn start_niri_monitor(tx: UnboundedSender<WindowManagerEvent>) {
    let niri = Niri::new();
    if !niri.is_available() {
        error!("Niri socket not found - is Niri running?");
        return;
    }
    niri.start_event_monitor(tx);
}

pub fn start_niri_monitor_sync(tx: Sender<WindowManagerEvent>) {
    let niri = Niri::new();
    if !niri.is_available() {
        error!("Niri socket not found - is Niri running?");
        return;
    }
    niri.start_event_monitor_sync(tx);
}

pub fn get_focused_window() -> WindowInfo {
    Niri::new().get_focused_window()
}
