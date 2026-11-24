#![allow(clippy::cast_possible_truncation)]

use crate::gamemode_state::{GlobalOverride, WindowOverride};
use crate::steno::overlay::{OverlayHub, OverlaySnapshot};
use crate::window_manager::WindowInfo;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// IPC message from client to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcRequest {
    /// Check if daemon is alive
    Ping,
    /// List all detected keyboards with their status
    ListKeyboards,
    /// Toggle enabled status of keyboards (returns current list for interactive selection)
    ToggleKeyboards,
    /// Enable specific keyboard by hardware ID
    EnableKeyboard(String),
    /// Disable specific keyboard by hardware ID
    DisableKeyboard(String),
    /// Reload configuration from disk
    Reload,
    /// Force save adaptive timing stats immediately
    SaveAdaptiveStats,
    /// Shutdown daemon
    Shutdown,

    /// Reported by a per-compositor watcher process on every focus change.
    /// The daemon resolves the effective game mode itself (heuristics +
    /// overrides) and applies it - watchers no longer compute a boolean.
    WindowFocusChanged {
        app_id: Option<String>,
        pid: Option<u32>,
        title: Option<String>,
    },
    /// Ask the daemon for the last window focus event it received (used by
    /// `keymux gamemode window` to default to "the currently focused app"
    /// without any compositor-specific code in the CLI).
    GetFocusedWindow,
    /// Set (or clear, with `state: None`) a temporary per-app_id override.
    SetWindowOverride {
        app_id: String,
        state: Option<WindowOverride>,
    },
    /// Flip the currently-effective state for an app_id into an explicit
    /// override (On -> Off, Off/heuristic-Normal -> On).
    ToggleWindowOverride { app_id: String },
    /// List all active per-app_id overrides
    ListWindowOverrides,
    /// Set the global override (Auto/AlwaysOn/AlwaysOff)
    SetGlobalOverride(GlobalOverride),
    /// Get the current global override
    GetGlobalOverride,
    /// Keep the connection open and stream [`OverlaySnapshot`] frames on every change.
    /// Handled by the IPC thread itself, never reaches the daemon's main loop.
    SubscribeOverlay,
    /// Which build the daemon is (`keymux --version` compares it with its own). Answered by
    /// the IPC thread itself, so it does not wait for the daemon's main loop.
    ///
    /// New requests go at the end: an older daemon decodes requests by position, and one it
    /// does not know makes it drop the connection, which the client reads as "older daemon".
    Version,
}

/// IPC response from daemon to client
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    /// Daemon is alive
    Pong,
    /// List of keyboards with their info
    KeyboardList(Vec<KeyboardInfo>),
    /// Operation succeeded
    Ok,
    /// Operation failed with error message
    Error(String),
    /// The last window the daemon saw focused (empty fields if none yet)
    FocusedWindow(WindowInfo),
    /// Active per-app_id overrides, sorted by app_id
    WindowOverrides(Vec<(String, WindowOverride)>),
    /// Current global override
    GlobalOverrideStatus(GlobalOverride),
    /// The daemon's version string, the same one its `keymux --version` prints
    Version(String),
}

/// Information about a detected keyboard
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyboardInfo {
    /// Persistent hardware ID including USB port (e.g. "362d:0210:0111:0003@3-4.2")
    pub hardware_id: String,
    /// Human-readable name
    pub name: String,
    /// Current device path (can change between boots)
    pub device_path: String,
    /// Whether this keyboard is currently enabled
    pub enabled: bool,
    /// Whether this keyboard is currently connected
    pub connected: bool,
    /// True when enabled via a portless config entry (e.g. "362d:0210:0111:0003")
    /// rather than an explicit port entry (e.g. "362d:0210:0111:0003@3-4.2").
    /// Used by the display layer to annotate the ID with a hint.
    pub enabled_by_portless: bool,
    /// The keyboard group this keyboard works in, if any (all its members share one processor)
    pub group: Option<String>,
    /// The config rule pattern that matched (e.g., "*", "1234", "Keychron")
    /// None if implicitly enabled/disabled (no explicit rule matched)
    pub matched_rule: Option<String>,
}

/// Get the IPC socket path for root daemon
pub fn get_root_socket_path() -> PathBuf {
    Path::new("/run").join("keymux.sock")
}

/// The user's runtime folder (`$XDG_RUNTIME_DIR`, or `/run/user/<uid>` under `sudo`)
pub(crate) fn user_runtime_dir() -> PathBuf {
    let (uid, _) = crate::get_actual_user_uid();
    std::env::var("XDG_RUNTIME_DIR")
        .map_or_else(|_| PathBuf::from(format!("/run/user/{uid}")), PathBuf::from)
}

/// Get the IPC socket path for user daemon (legacy, for compatibility)
pub(crate) fn get_user_socket_path() -> PathBuf {
    user_runtime_dir().join("keymux.sock")
}

/// Get the IPC socket path (tries root first, falls back to user)
pub(crate) fn get_socket_path() -> PathBuf {
    let root_sock = get_root_socket_path();
    if root_sock.exists() {
        root_sock
    } else {
        get_user_socket_path()
    }
}

/// Shown when the daemon cannot make sense of a request: it is older than this binary.
const OLD_DAEMON: &str = "the keymux daemon did not understand this request: it is probably older than this binary. Restart it (systemctl restart keymux; or your init system) and try again.";

/// Send an IPC request and receive response
pub fn send_request(request: &IpcRequest) -> Result<IpcResponse> {
    let socket_path = get_socket_path();
    let stream =
        UnixStream::connect(&socket_path).context("Failed to connect to daemon. Is it running?")?;
    exchange(stream, request)
}

/// One request/response round trip on a connected socket.
///
/// A daemon from before an upgrade drops the connection (or answers with something this binary
/// cannot decode) when it gets a request it does not know, so a failure to read the reply is
/// reported as that, with the low-level error kept as the cause.
fn exchange(mut stream: UnixStream, request: &IpcRequest) -> Result<IpcResponse> {
    // Serialize and send request
    let encoded = bincode::serialize(request)?;
    let len = (encoded.len() as u32).to_le_bytes();
    stream.write_all(&len)?;
    stream.write_all(&encoded)?;
    stream.flush()?;

    read_reply(&mut stream).map_err(|e| e.context(OLD_DAEMON))
}

fn read_reply(stream: &mut UnixStream) -> Result<IpcResponse> {
    // Read response length
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;

    // Read response
    let mut response_buf = vec![0u8; len];
    stream.read_exact(&mut response_buf)?;
    Ok(bincode::deserialize(&response_buf)?)
}

/// One quiet request/response round trip on the socket at `path`, bounded by `timeout`.
///
/// The timeout applies to sending and to receiving, and nothing is reported on failure: for
/// callers that must stay fast and silent whatever the daemon does, such as
/// `keymux --version`.
pub fn request_with_timeout(
    path: &Path,
    request: &IpcRequest,
    timeout: Duration,
) -> Option<IpcResponse> {
    let mut stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;
    write_frame(&mut stream, request).ok()?;
    read_frame(&mut stream)
}

/// The requests the daemon's IPC thread answers by itself, without the main loop: the answer
/// does not depend on the daemon's state, so it comes back at once even during a reload.
pub fn local_reply(request: &IpcRequest) -> Option<IpcResponse> {
    match request {
        IpcRequest::Version => Some(IpcResponse::Version(crate::running::VERSION.to_string())),
        _ => None,
    }
}

/// Daemon side: send one response.
pub fn reply(stream: &mut impl Write, response: &IpcResponse) -> Result<()> {
    write_frame(stream, response)
}

/// Daemon side: read one request, `None` when the stream ends or the request is not one this
/// build knows.
pub fn read_request(stream: &mut impl Read) -> Option<IpcRequest> {
    read_frame(stream)
}

/// Write one length-prefixed bincode frame, the same framing as requests and responses.
fn write_frame(stream: &mut impl Write, value: &impl Serialize) -> Result<()> {
    let encoded = bincode::serialize(value)?;
    stream.write_all(&(encoded.len() as u32).to_le_bytes())?;
    stream.write_all(&encoded)?;
    stream.flush()?;
    Ok(())
}

/// Daemon side of [`IpcRequest::SubscribeOverlay`]: send the current state, then every newer one.
///
/// Runs on its own thread until the subscriber goes away, and never touches the key path except
/// through the hub's lock.
///
/// The subscriber never sends anything after subscribing, so a second thread blocks reading
/// the socket: it returns the moment the overlay closes its end, and wakes the writer. That
/// way a closed overlay is noticed at once, not at the next write (which can still succeed
/// once after the peer has gone) and with no idle wakeups.
pub fn serve_overlay(mut stream: UnixStream, hub: &OverlayHub) {
    let Ok(mut watcher) = stream.try_clone() else {
        return;
    };
    let gone = AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut ignored = [0u8; 64];
            while matches!(watcher.read(&mut ignored), Ok(n) if n > 0) {}
            gone.store(true, Ordering::Release);
            hub.wake();
        });

        let mut state = hub.snapshot();
        'serving: loop {
            if write_frame(&mut stream, &state).is_err() {
                break;
            }
            // Wait for something newer; the long timeout only bounds a single wait
            loop {
                if gone.load(Ordering::Acquire) {
                    break 'serving;
                }
                if let Some(newer) =
                    hub.wait_newer(state.generation, Duration::from_secs(3600), &gone)
                {
                    state = newer;
                    break;
                }
            }
        }
        // Ends the watcher's blocking read too, if it is still running (a failed write)
        let _ = stream.shutdown(std::net::Shutdown::Both);
    });
}

/// Overlay side: connect, subscribe, and return the frames as they arrive. The iterator
/// ends when the daemon goes away; the caller reconnects.
pub fn subscribe_overlay() -> Result<impl Iterator<Item = OverlaySnapshot>> {
    let mut stream = UnixStream::connect(get_socket_path())
        .context("Failed to connect to daemon. Is it running?")?;
    write_frame(&mut stream, &IpcRequest::SubscribeOverlay)?;
    Ok(std::iter::from_fn(move || read_frame(&mut stream)))
}

/// How often a helper checks that the daemon is still there.
const PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Consecutive failed checks before a helper decides the daemon is gone. The daemon answers a
/// Ping from its main loop, so a reload can make one check late; three in a row is a real loss.
const LOSS_MISSES: u32 = 3;
/// How long a single check waits for the Pong.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Is a keymux daemon answering on the socket at `path`? Connects and Pings, because the
/// socket file outlives a killed daemon: a stale file connects to nothing.
pub fn probe_socket(path: &Path) -> bool {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return false;
    };
    if stream.set_read_timeout(Some(PROBE_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(PROBE_TIMEOUT)).is_err()
        || write_frame(&mut stream, &IpcRequest::Ping).is_err()
    {
        return false;
    }
    matches!(
        read_frame::<IpcResponse>(&mut stream),
        Some(IpcResponse::Pong)
    )
}

/// Is the daemon answering on its socket right now?
pub fn daemon_alive() -> bool {
    probe_socket(&get_socket_path())
}

/// Poll `probe` every `interval` until it succeeds. `timeout` of `None` waits forever.
/// Returns whether the probe succeeded. Sleeps between checks, never spins.
pub fn wait_until(
    mut probe: impl FnMut() -> bool,
    interval: Duration,
    timeout: Option<Duration>,
) -> bool {
    let started = std::time::Instant::now();
    loop {
        if probe() {
            return true;
        }
        if timeout.is_some_and(|limit| started.elapsed() >= limit) {
            return false;
        }
        std::thread::sleep(interval);
    }
}

/// `keymux wait-daemon`: block until the daemon answers. What the init systems run before
/// starting a helper, so the helper never runs without a daemon to talk to.
pub fn wait_for_daemon(timeout: Option<Duration>) -> bool {
    wait_until(daemon_alive, Duration::from_millis(500), timeout)
}

/// Decides when the daemon counts as lost. Only a daemon that was seen alive can be lost: a
/// helper started by hand before the daemon keeps waiting, as it always did.
#[derive(Debug, Default)]
pub struct LossTracker {
    seen_alive: bool,
    misses: u32,
}

impl LossTracker {
    /// Feed one check's result. `true` means the daemon is lost: stop and let the init
    /// system start us again once the daemon is back.
    pub const fn observe(&mut self, alive: bool) -> bool {
        if alive {
            self.seen_alive = true;
            self.misses = 0;
            false
        } else {
            self.misses += 1;
            self.seen_alive && self.misses >= LOSS_MISSES
        }
    }
}

/// Daemon-loss check for a helper that is a daemon client (the window watchers).
///
/// Call [`check`] from the main loop as often as you like; it probes at most once a second and
/// returns an error when the daemon is gone, which the helper returns from `main` so it exits
/// non-zero. The init system restarts it when the daemon is back (see "Services" in the
/// README).
///
/// Exit rather than reconnect: the daemon keeps per-helper state (the last focused window),
/// so a fresh helper reports it again at start; and an exited helper is what makes
/// "daemon stopped" visible to the init system instead of a helper idling disconnected.
///
/// [`check`]: DaemonWatch::check
pub struct DaemonWatch {
    tracker: LossTracker,
    last: std::time::Instant,
}

impl Default for DaemonWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonWatch {
    pub fn new() -> Self {
        let mut watch = Self {
            tracker: LossTracker::default(),
            last: std::time::Instant::now(),
        };
        watch.tracker.observe(daemon_alive());
        watch
    }

    pub fn check(&mut self) -> Result<()> {
        if self.last.elapsed() < PROBE_INTERVAL {
            return Ok(());
        }
        self.last = std::time::Instant::now();
        if self.tracker.observe(daemon_alive()) {
            anyhow::bail!("the keymux daemon went away, exiting so it can be restarted with it");
        }
        Ok(())
    }
}

/// The next length-prefixed frame, or `None` when the stream ends or breaks.
fn read_frame<T: serde::de::DeserializeOwned>(stream: &mut impl Read) -> Option<T> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len).ok()?;
    let mut frame = vec![0u8; u32::from_le_bytes(len) as usize];
    stream.read_exact(&mut frame).ok()?;
    bincode::deserialize(&frame).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_with(bytes: Vec<u8>) -> Result<IpcResponse> {
        let (client, mut server) = UnixStream::pair().unwrap();
        let fake = std::thread::spawn(move || {
            let mut len = [0u8; 4];
            server.read_exact(&mut len).unwrap();
            let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
            server.read_exact(&mut body).unwrap();
            server.write_all(&bytes).unwrap();
        });
        let result = exchange(client, &IpcRequest::Ping);
        fake.join().unwrap();
        result
    }

    #[test]
    fn a_daemon_that_closes_early_is_reported_as_older() {
        let err = reply_with(Vec::new()).unwrap_err();
        assert_eq!(err.to_string(), OLD_DAEMON);
        assert!(format!("{err:?}").contains("failed to fill whole buffer"));
    }

    #[test]
    fn a_daemon_that_sends_garbage_is_reported_as_older() {
        let mut bytes = 3u32.to_le_bytes().to_vec();
        bytes.extend([0xff, 0xff, 0xff]);
        let err = reply_with(bytes).unwrap_err();
        assert_eq!(err.to_string(), OLD_DAEMON);
    }

    #[test]
    fn a_subscriber_gets_the_current_state_then_each_change() {
        let hub = OverlayHub::default();
        hub.set_layer(Some("steno"));
        let (mut client, server) = UnixStream::pair().unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| serve_overlay(server, &hub));

            let first: OverlaySnapshot = read_frame(&mut client).unwrap();
            assert_eq!(first.layer.as_deref(), Some("steno"));

            hub.set_keys(0b11, 0b11);
            let next: OverlaySnapshot = read_frame(&mut client).unwrap();
            assert_eq!((next.chord, next.held), (0b11, 0b11));
            assert!(next.generation > first.generation);

            // Closing our end must stop the stream thread on its own, with no further change
            // to wake it, so the scope can end. If it didn't, this test would hang.
            drop(client);
        });
    }

    #[test]
    fn a_subscriber_that_stops_listening_is_noticed_without_any_change() {
        let hub = OverlayHub::default();
        let (client, server) = UnixStream::pair().unwrap();
        let started = std::time::Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| serve_overlay(server, &hub));
            // Let it send its first frame and settle into waiting, then hang up
            let mut client = client;
            let _: OverlaySnapshot = read_frame(&mut client).unwrap();
            drop(client);
        });
        // A heartbeat design would take its whole interval here; this must be near instant
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn the_request_round_trips_in_the_same_framing() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        write_frame(&mut a, &IpcRequest::SubscribeOverlay).unwrap();
        assert!(matches!(
            read_frame::<IpcRequest>(&mut b),
            Some(IpcRequest::SubscribeOverlay)
        ));
    }

    /// A socket path in a fresh folder of its own; the folder goes when the guard does.
    fn temp_socket(name: &str) -> (crate::test_dir::TestDir, PathBuf) {
        let dir = crate::test_dir::TestDir::new(&format!("keymux-test-{name}"));
        let path = dir.join("s.sock");
        (dir, path)
    }

    #[test]
    fn probe_sees_a_live_daemon_and_not_a_stale_socket_file() {
        let (_dir, path) = temp_socket("probe");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            assert!(matches!(
                read_frame::<IpcRequest>(&mut conn),
                Some(IpcRequest::Ping)
            ));
            write_frame(&mut conn, &IpcResponse::Pong).unwrap();
            // The listener drops here, leaving the socket file behind like a killed daemon
        });
        assert!(probe_socket(&path));
        server.join().unwrap();
        assert!(path.exists(), "the file stays after the daemon is gone");
        assert!(!probe_socket(&path), "a stale socket file is not a daemon");
        std::fs::remove_file(&path).unwrap();
        assert!(!probe_socket(&path), "no file is not a daemon");
    }

    #[test]
    fn a_connection_that_never_answers_is_not_a_daemon() {
        let (_dir, path) = temp_socket("silent");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let started = std::time::Instant::now();
        assert!(!probe_socket(&path));
        assert!(started.elapsed() < Duration::from_secs(10));
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn loss_needs_a_daemon_seen_alive_and_several_misses_in_a_row() {
        let mut never_seen = LossTracker::default();
        assert!(
            (0..10).all(|_| !never_seen.observe(false)),
            "waits for a daemon that never came"
        );

        let mut tracker = LossTracker::default();
        assert!(!tracker.observe(true));
        assert!(!tracker.observe(false));
        assert!(!tracker.observe(false));
        assert!(!tracker.observe(true), "a recovery resets the count");
        assert!(!tracker.observe(false));
        assert!(!tracker.observe(false));
        assert!(tracker.observe(false), "the third miss in a row is a loss");
    }

    #[test]
    fn wait_until_returns_when_the_probe_succeeds_and_gives_up_at_the_timeout() {
        let mut calls = 0;
        assert!(wait_until(
            || {
                calls += 1;
                calls == 3
            },
            Duration::from_millis(1),
            None
        ));
        assert_eq!(calls, 3);

        let started = std::time::Instant::now();
        assert!(!wait_until(
            || false,
            Duration::from_millis(5),
            Some(Duration::from_millis(30))
        ));
        assert!(started.elapsed() >= Duration::from_millis(30));
    }
}
