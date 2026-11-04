use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowInfo {
    pub app_id: Option<String>,
    pub pid: Option<u32>,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub enum WindowManagerEvent {
    WindowFocusChanged(WindowInfo),
}

/// Where a monitor sends the events it reads. `false` once nobody is listening any more.
pub trait EventSink: Send + 'static {
    fn send_event(&self, event: WindowManagerEvent) -> bool;
}

impl EventSink for Sender<WindowManagerEvent> {
    fn send_event(&self, event: WindowManagerEvent) -> bool {
        self.send(event).is_ok()
    }
}

impl EventSink for crossbeam_channel::Sender<WindowManagerEvent> {
    fn send_event(&self, event: WindowManagerEvent) -> bool {
        self.send(event).is_ok()
    }
}

pub trait WindowManager: Send + Sync + Clone + 'static {
    fn name(&self) -> &'static str;

    fn is_available(&self) -> bool;

    fn detect_socket(&self) -> Option<PathBuf>;

    fn get_focused_window(&self) -> WindowInfo;

    fn event_stream_args(&self) -> Vec<&'static str>;

    fn parse_event(&self, line: &str) -> Option<WindowInfo>;

    fn start_event_monitor_sync(&self, tx: impl EventSink)
    where
        Self: Sized,
    {
        let wm: &'static Self = Box::leak(Box::new(self.clone()));
        spawn_event_monitor_sync(wm, tx);
    }
}

pub(crate) fn spawn_event_monitor_sync<T: WindowManager + 'static>(
    wm: &'static T,
    tx: impl EventSink,
) {
    if wm.detect_socket().is_none() {
        error!("{}: No socket found, cannot start monitor", wm.name());
        return;
    }

    thread::spawn(move || loop {
        info!("{}: Starting event stream monitor...", wm.name());

        let mut child = match std::process::Command::new(wm.name())
            .args(wm.event_stream_args())
            .stdout(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                error!("{}: Failed to spawn: {}", wm.name(), e);
                thread::sleep(Duration::from_secs(5));
                continue;
            }
        };

        let Some(stdout) = child.stdout.take() else {
            error!("{}: Failed to capture stdout", wm.name());
            thread::sleep(Duration::from_secs(5));
            continue;
        };

        use std::io::{BufRead, BufReader};
        let reader = BufReader::new(stdout);

        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if let Some(window_info) = wm.parse_event(&line) {
                        if let Some(ref app) = window_info.app_id {
                            debug!(
                                "{}: Focus changed → app_id: {}, pid: {:?}",
                                wm.name(),
                                app,
                                window_info.pid
                            );
                        }
                        if !tx.send_event(WindowManagerEvent::WindowFocusChanged(window_info)) {
                            error!("{}: Channel closed, exiting", wm.name());
                            return;
                        }
                    }
                }
                Err(e) => {
                    warn!("{}: Error reading event: {}", wm.name(), e);
                    break;
                }
            }
        }

        let _ = child.wait();

        error!(
            "{}: Event stream ended, restarting in 5 seconds...",
            wm.name()
        );
        thread::sleep(Duration::from_secs(5));
    });
}
