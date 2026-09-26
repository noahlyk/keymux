use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::Sender;
use tokio::sync::mpsc::UnboundedSender;
use tracing::error;

use crate::window_manager::{WindowInfo, WindowManager, WindowManagerEvent};

#[derive(Clone)]
pub struct I3WindowManager;

impl I3WindowManager {
    pub const fn new() -> Self {
        Self
    }

    fn get_ipc_socket_path() -> Option<PathBuf> {
        if let Ok(socket_path) = std::env::var("I3SOCK") {
            let path = PathBuf::from(&socket_path);
            if path.exists() {
                return Some(path);
            }
        }

        if let Ok(socket_path) = std::env::var("SWAYSOCK") {
            let path = PathBuf::from(&socket_path);
            if path.exists() {
                return Some(path);
            }
        }

        let home = std::env::var("HOME").ok()?;
        let socket_path = PathBuf::from(home).join(".i3").join("ipc.sock");
        if socket_path.exists() {
            return Some(socket_path);
        }

        None
    }

    fn get_focused_window_info() -> WindowInfo {
        let output = Command::new("i3-msg")
            .args(["-t", "getFocusedWindow", "-r"])
            .output();

        match output {
            Ok(output) if output.status.success() => {
                let Ok(json_str) = String::from_utf8(output.stdout) else {
                    return WindowInfo {
                        app_id: None,
                        pid: None,
                        title: None,
                    };
                };

                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    let app_id = json
                        .get("window_properties")
                        .and_then(|v| v.get("class"))
                        .or_else(|| {
                            json.get("window_properties")
                                .and_then(|v| v.get("instance"))
                        })
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    let pid = json.get("pid").and_then(|v| v.as_u64()).map(|v| v as u32);

                    let title = json
                        .get("name")
                        .or_else(|| json.get("window_properties").and_then(|v| v.get("title")))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    return WindowInfo { app_id, pid, title };
                }

                WindowInfo {
                    app_id: None,
                    pid: None,
                    title: None,
                }
            }
            _ => WindowInfo {
                app_id: None,
                pid: None,
                title: None,
            },
        }
    }
}

impl Default for I3WindowManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowManager for I3WindowManager {
    fn name(&self) -> &'static str {
        "i3"
    }

    fn is_available(&self) -> bool {
        Self::get_ipc_socket_path().is_some()
    }

    fn detect_socket(&self) -> Option<PathBuf> {
        Self::get_ipc_socket_path()
    }

    fn get_focused_window(&self) -> WindowInfo {
        Self::get_focused_window_info()
    }

    fn event_stream_args(&self) -> Vec<&'static str> {
        vec!["-t", "subscribe", "-r", "[\"window\"]"]
    }

    fn parse_event(&self, line: &str) -> Option<WindowInfo> {
        if line.contains("\"change\":\"focus\"") || line.contains("\"change\":\"new\"") {
            Some(Self::get_focused_window_info())
        } else {
            None
        }
    }
}

#[derive(Clone)]
pub struct BspwmWindowManager;

impl BspwmWindowManager {
    pub const fn new() -> Self {
        Self
    }

    fn get_focused_window_info() -> WindowInfo {
        let output = Command::new("bspc")
            .args(["query", "-T", "-m", "focused"])
            .output();

        match output {
            Ok(output) if output.status.success() => {
                let Ok(json_str) = String::from_utf8(output.stdout) else {
                    return WindowInfo {
                        app_id: None,
                        pid: None,
                        title: None,
                    };
                };

                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    let app_id = json
                        .get("client")
                        .and_then(|v| v.get("className"))
                        .or_else(|| json.get("client").and_then(|v| v.get("instance")))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    let pid = json
                        .get("client")
                        .and_then(|v| v.get("pid"))
                        .and_then(|v| v.as_u64())
                        .map(|v| v as u32);

                    let title = json
                        .get("client")
                        .and_then(|v| v.get("title"))
                        .or_else(|| json.get("client").and_then(|v| v.get("name")))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    return WindowInfo { app_id, pid, title };
                }

                WindowInfo {
                    app_id: None,
                    pid: None,
                    title: None,
                }
            }
            _ => WindowInfo {
                app_id: None,
                pid: None,
                title: None,
            },
        }
    }
}

impl Default for BspwmWindowManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowManager for BspwmWindowManager {
    fn name(&self) -> &'static str {
        "bspc"
    }

    fn is_available(&self) -> bool {
        Command::new("bspc")
            .arg("query")
            .arg("-M")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn detect_socket(&self) -> Option<PathBuf> {
        None
    }

    fn get_focused_window(&self) -> WindowInfo {
        Self::get_focused_window_info()
    }

    fn event_stream_args(&self) -> Vec<&'static str> {
        vec!["subscribe", "window_focus", "window_create"]
    }

    fn parse_event(&self, line: &str) -> Option<WindowInfo> {
        if line.starts_with("window_focus") || line.starts_with("window_create") {
            Some(Self::get_focused_window_info())
        } else {
            None
        }
    }
}

pub fn is_i3_available() -> bool {
    I3WindowManager::new().is_available()
}

pub fn is_bspwm_available() -> bool {
    BspwmWindowManager::new().is_available()
}

// detect_x11_wm removed - not needed for basic functionality

pub fn start_i3_monitor(tx: UnboundedSender<WindowManagerEvent>) {
    let wm = I3WindowManager::new();
    if !wm.is_available() {
        error!("i3 socket not found - is i3 running?");
        return;
    }
    wm.start_event_monitor(tx);
}

pub fn start_i3_monitor_sync(tx: Sender<WindowManagerEvent>) {
    let wm = I3WindowManager::new();
    if !wm.is_available() {
        error!("i3 socket not found - is i3 running?");
        return;
    }
    wm.start_event_monitor_sync(tx);
}

pub fn start_bspwm_monitor(tx: UnboundedSender<WindowManagerEvent>) {
    let wm = BspwmWindowManager::new();
    if !wm.is_available() {
        error!("bspwm not available - is bspwm running?");
        return;
    }
    wm.start_event_monitor(tx);
}

pub fn start_bspwm_monitor_sync(tx: Sender<WindowManagerEvent>) {
    let wm = BspwmWindowManager::new();
    if !wm.is_available() {
        error!("bspwm not available - is bspwm running?");
        return;
    }
    wm.start_event_monitor_sync(tx);
}

pub fn get_focused_window() -> WindowInfo {
    if is_i3_available() {
        I3WindowManager::new().get_focused_window()
    } else if is_bspwm_available() {
        BspwmWindowManager::new().get_focused_window()
    } else {
        WindowInfo {
            app_id: None,
            pid: None,
            title: None,
        }
    }
}
