use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::Sender;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info};

use crate::window_manager::{WindowInfo, WindowManager, WindowManagerEvent};

#[derive(Clone)]
pub struct WaylandCompositor {
    pub wm_name: &'static str,
    pub cli_command: &'static str,
    pub socket_env_var: &'static str,
    pub socket_dir: &'static str,
    pub active_window_args: &'static [&'static str],
    pub subscribe_event: &'static str,
}

impl WaylandCompositor {
    pub const fn hyprland() -> Self {
        Self {
            wm_name: "hyprland",
            cli_command: "hyprctl",
            socket_env_var: "HYPRLAND_INSTANCE_SIGNATURE",
            socket_dir: "hypr",
            active_window_args: &["activewindow", "-j"],
            subscribe_event: "activewindow",
        }
    }

    pub const fn sway() -> Self {
        Self {
            wm_name: "sway",
            cli_command: "swaymsg",
            socket_env_var: "SWAYSOCK",
            socket_dir: "sway",
            active_window_args: &["-t", "getFocusedWindow", "-r"],
            subscribe_event: "window",
        }
    }

    fn detect_socket(&self) -> Option<PathBuf> {
        if let Ok(socket_path) = std::env::var(self.socket_env_var) {
            let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
                });

            let wm_dir = runtime_dir.join(self.socket_dir).join(&socket_path);
            if wm_dir.exists() {
                if let Ok(entries) = fs::read_dir(&wm_dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|s| s.to_str()) == Some("sock") {
                            info!("Found {} socket via env: {}", self.wm_name, path.display());
                            return Some(path);
                        }
                    }
                }
            }
        }

        let uid = unsafe { libc::getuid() };
        let runtime_dir = format!("/run/user/{}/{}", uid, self.socket_dir);

        if let Ok(entries) = fs::read_dir(&runtime_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if let Ok(subentries) = fs::read_dir(&path) {
                        for subentry in subentries.flatten() {
                            let subpath = subentry.path();
                            if subpath.extension().and_then(|s| s.to_str()) == Some("sock") {
                                info!(
                                    "Found {} socket via scan: {}",
                                    self.wm_name,
                                    subpath.display()
                                );
                                return Some(subpath);
                            }
                        }
                    }
                }
            }
        }

        None
    }

    fn get_focused_window_info(&self) -> WindowInfo {
        let output = Command::new(self.cli_command)
            .args(self.active_window_args)
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
                        .get("app_id")
                        .or_else(|| json.get("class"))
                        .or_else(|| json.get("appid"))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    let pid = json.get("pid").and_then(|v| v.as_u64()).map(|v| v as u32);

                    let title = json
                        .get("title")
                        .or_else(|| json.get("name"))
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

impl WindowManager for WaylandCompositor {
    fn name(&self) -> &'static str {
        self.wm_name
    }

    fn is_available(&self) -> bool {
        self.detect_socket().is_some()
    }

    fn detect_socket(&self) -> Option<PathBuf> {
        self.detect_socket()
    }

    fn get_focused_window(&self) -> WindowInfo {
        self.get_focused_window_info()
    }

    fn event_stream_args(&self) -> Vec<&'static str> {
        vec!["subscribe", self.subscribe_event]
    }

    fn parse_event(&self, line: &str) -> Option<WindowInfo> {
        if line.contains(self.subscribe_event) && !line.contains("{\"success\":true") {
            Some(self.get_focused_window_info())
        } else {
            None
        }
    }
}

pub fn is_hyprland_available() -> bool {
    WaylandCompositor::hyprland().is_available()
}

pub fn is_sway_available() -> bool {
    WaylandCompositor::sway().is_available()
}

pub fn detect_wayland_compositor() -> Option<WaylandCompositor> {
    if is_hyprland_available() {
        return Some(WaylandCompositor::hyprland());
    }
    if is_sway_available() {
        return Some(WaylandCompositor::sway());
    }
    None
}

pub fn start_hyprland_monitor(tx: UnboundedSender<WindowManagerEvent>) {
    let wm = WaylandCompositor::hyprland();
    if !wm.is_available() {
        error!("Hyprland socket not found - is Hyprland running?");
        return;
    }
    wm.start_event_monitor(tx);
}

pub fn start_hyprland_monitor_sync(tx: Sender<WindowManagerEvent>) {
    let wm = WaylandCompositor::hyprland();
    if !wm.is_available() {
        error!("Hyprland socket not found - is Hyprland running?");
        return;
    }
    wm.start_event_monitor_sync(tx);
}

pub fn start_sway_monitor(tx: UnboundedSender<WindowManagerEvent>) {
    let wm = WaylandCompositor::sway();
    if !wm.is_available() {
        error!("Sway socket not found - is Sway running?");
        return;
    }
    wm.start_event_monitor(tx);
}

pub fn start_sway_monitor_sync(tx: Sender<WindowManagerEvent>) {
    let wm = WaylandCompositor::sway();
    if !wm.is_available() {
        error!("Sway socket not found - is Sway running?");
        return;
    }
    wm.start_event_monitor_sync(tx);
}

pub fn get_focused_window() -> WindowInfo {
    detect_wayland_compositor().map_or(
        WindowInfo {
            app_id: None,
            pid: None,
            title: None,
        },
        |wm| wm.get_focused_window(),
    )
}
