/// The daemon: one loop that reacts to hotplug, IPC, window focus, config changes and
/// session changes, each arriving from a thread of its own, while every keyboard's event
/// processor runs on its own thread and never waits on any of it.
use crate::config::ConfigManager;
use crate::event_processor;
use crate::gamemode_state::{GlobalOverride, WindowOverride};
use crate::ipc::{get_root_socket_path, IpcRequest, IpcResponse};
use crate::keyboard_id::{find_all_keyboards, KeyboardId};
use crate::niri::gamemode_detection::resolve_effective_game_mode;
use crate::session_manager::SessionManager;
use crate::window_manager::WindowInfo;
use anyhow::{Context, Result};
use crossbeam_channel::{at, never, select, tick, unbounded, Receiver, Sender};
use evdev::Device;
use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, info, warn};

/// Metadata about a keyboard
#[derive(Debug, Clone)]
struct KeyboardMeta {
    name: String,
    /// ALL event file paths for this logical keyboard
    paths: Vec<PathBuf>,
    connected: bool,
}

/// Active event processor thread handle
struct ProcessorHandle {
    shutdown_tx: crossbeam_channel::Sender<()>,
    game_mode_tx: mpsc::Sender<bool>,
    save_stats_tx: mpsc::Sender<()>,
    thread_handle: Option<thread::JoinHandle<()>>,
}

/// A thread feeding the daemon's loop, named so a dead one can be taken out of the select
/// instead of being selected on forever.
#[derive(Clone, Copy)]
enum Source {
    Hotplug,
    Ipc,
    Niri,
    ConfigWatch,
}

/// A source thread ended: say so, and take its place with a receiver that never fires.
fn ended<T>(name: &str) -> Receiver<T> {
    warn!("{name} thread ended; the daemon carries on without it");
    never()
}

pub struct Daemon {
    /// Per-user configuration managers (uid -> ConfigManager)
    user_configs: HashMap<u32, ConfigManager>,
    /// Session manager for multi-user support
    session_manager: SessionManager,
    /// All detected keyboards
    all_keyboards: HashMap<KeyboardId, KeyboardMeta>,
    /// Active event processors - ONE THREAD PER EVENT FILE (event_path -> (keyboard_id, uid, handle))
    active_processors: HashMap<PathBuf, (KeyboardId, u32, ProcessorHandle)>,
    /// Keyboard ownership (keyboard_id -> uid)
    keyboard_owners: HashMap<KeyboardId, u32>,
    /// Current game mode state (preserved across thread restarts)
    game_mode_active: bool,
    /// Receiver for processor thread death notifications (path of the dead processor)
    processor_dead_rx: Receiver<PathBuf>,
    /// Sender side kept on the daemon to clone into each new ProcessorHandle
    processor_dead_tx: Sender<PathBuf>,

    /// System-wide game mode override (`keymux gamemode global`). Ignores
    /// per-window heuristics/overrides entirely when not `Auto`. Session-only
    /// - resets to `Auto` on every daemon restart.
    global_override: GlobalOverride,
    /// Temporary per-app_id game mode overrides (`keymux gamemode window`).
    /// Session-only - resets to empty on every daemon restart.
    window_overrides: HashMap<String, WindowOverride>,
    /// The most recent window focus event seen from any watcher, used to
    /// resolve "the currently focused window" for CLI commands and to
    /// recompute game mode immediately when an override changes.
    last_focused_window: Option<WindowInfo>,
    /// UIDs we've already warned about having no config file, so the
    /// warning is logged once per session rather than every 5-second
    /// session-refresh tick.
    warned_missing_config: HashSet<u32>,
}

impl Daemon {
    pub fn new(_config_path: Option<PathBuf>, _user: Option<String>) -> Result<Self> {
        info!("Initializing async keyboard middleware daemon");

        // Check if running as root
        let is_root = unsafe { libc::getuid() } == 0;
        if !is_root {
            return Err(anyhow::anyhow!(
                "Daemon must run as root for device access. Use 'sudo systemctl start keymux'"
            ));
        }

        // /dev/uinput is required to create the virtual output device for
        // every keyboard processor. Fail fast with an actionable message
        // here, instead of a raw per-keyboard OS-error log line the first
        // time a keyboard is plugged in and enabled.
        if let Err(e) = std::fs::OpenOptions::new().write(true).open("/dev/uinput") {
            return Err(anyhow::anyhow!(
                "/dev/uinput not accessible ({e}). The uinput kernel module is required for key remapping. Try: sudo modprobe uinput"
            ));
        }

        let session_manager = SessionManager::new();
        let (processor_dead_tx, processor_dead_rx) = unbounded();

        Ok(Self {
            user_configs: HashMap::new(),
            session_manager,
            all_keyboards: HashMap::new(),
            active_processors: HashMap::new(),
            keyboard_owners: HashMap::new(),
            game_mode_active: false,
            processor_dead_rx,
            processor_dead_tx,
            global_override: GlobalOverride::default(),
            window_overrides: HashMap::new(),
            last_focused_window: None,
            warned_missing_config: HashSet::new(),
        })
    }

    /// Run the daemon's event loop.
    pub fn run(&mut self) -> Result<()> {
        info!("Starting keyboard middleware daemon (multi-user mode)");

        // Start background services
        let mut hotplug_rx = self.start_hotplug_monitor();
        let mut ipc_rx = self.start_ipc_server()?;
        let mut niri_rx = self.start_niri_monitor();
        let mut config_watch_rx = self.start_config_watcher();
        let processor_dead_rx = self.processor_dead_rx.clone();

        // Initial session and keyboard discovery
        info!("Refreshing user sessions...");
        self.refresh_sessions();

        info!("Discovering keyboards...");
        self.discover_keyboards();

        // Load user configs and sync keyboards
        info!("Loading user configs...");
        self.load_user_configs();

        info!("Syncing keyboards to users...");
        self.sync_keyboards_to_users();

        let session_check = tick(Duration::from_secs(5));
        // A single physical replug fires many udev events in rapid succession (with --udev,
        // each only after udev's rules ran, so the device node is ready): the resync waits
        // until they have settled
        const HOTPLUG_DEBOUNCE: Duration = Duration::from_millis(300);
        let mut hotplug_settle: Option<Instant> = None;

        loop {
            let settle = hotplug_settle.map_or_else(never, at);
            let mut closed: Option<Source> = None;
            select! {
                recv(hotplug_rx) -> event => match event {
                    Ok(event) => {
                        if event.contains(" add ") || event.contains(" remove ") {
                            debug!("Hotplug event (add/remove): {}", event);
                            hotplug_settle = Some(Instant::now() + HOTPLUG_DEBOUNCE);
                        }
                    }
                    Err(_) => closed = Some(Source::Hotplug),
                },
                recv(settle) -> _ => {
                    // Drain any events that arrived during the debounce window
                    while let Ok(event) = hotplug_rx.try_recv() {
                        debug!("Draining queued hotplug event: {}", event);
                    }
                    hotplug_settle = None;
                    info!("Hotplug settled, resyncing keyboards...");
                    self.refresh_sessions();
                    self.load_user_configs();
                    self.discover_keyboards();
                    self.sync_keyboards_to_users();
                }
                recv(ipc_rx) -> message => match message {
                    Ok((request, resp_tx)) => {
                        debug!("IPC request: {:?}", request);
                        let response = self.handle_ipc_request(request);
                        let _ = resp_tx.send(response);
                    }
                    Err(_) => closed = Some(Source::Ipc),
                },
                recv(niri_rx) -> event => match event {
                    Ok(event) => self.process_niri_event(event),
                    Err(_) => closed = Some(Source::Niri),
                },
                recv(config_watch_rx) -> changed => match changed {
                    Ok(()) => self.config_file_changed(),
                    Err(_) => closed = Some(Source::ConfigWatch),
                },
                recv(session_check) -> _ => {
                    self.refresh_sessions();
                    self.sync_keyboards_to_users();
                }
                recv(processor_dead_rx) -> dead => {
                    if let Ok(dead_path) = dead {
                        self.processor_died(&dead_path);
                    }
                }
            }
            match closed {
                None => {}
                Some(Source::Hotplug) => hotplug_rx = ended("hotplug monitor"),
                Some(Source::Ipc) => ipc_rx = ended("IPC server"),
                Some(Source::Niri) => niri_rx = ended("niri monitor"),
                Some(Source::ConfigWatch) => config_watch_rx = ended("config watcher"),
            }
        }
    }

    /// Discover all keyboards (updates metadata only, doesn't start processors)
    fn discover_keyboards(&mut self) {
        info!("Discovering keyboards...");

        let keyboards = find_all_keyboards();
        info!("Found {} logical keyboard(s)", keyboards.len());

        // Mark all existing keyboards as disconnected first
        for meta in self.all_keyboards.values_mut() {
            meta.connected = false;
        }

        // Update keyboard metadata for connected keyboards
        for (kbd_id, logical_kbd) in keyboards {
            let kbd_name = logical_kbd.name.clone();
            let paths: Vec<PathBuf> = logical_kbd
                .devices
                .iter()
                .map(|(path, _)| path.clone())
                .collect();

            let was_known = self.all_keyboards.contains_key(&kbd_id);
            info!(
                "{} keyboard: {} ({}) with {} event file(s)",
                if was_known {
                    "Found existing"
                } else {
                    "Detected new"
                },
                kbd_name,
                kbd_id,
                paths.len()
            );

            self.all_keyboards.insert(
                kbd_id.clone(),
                KeyboardMeta {
                    name: kbd_name,
                    paths,
                    connected: true,
                },
            );
        }

        // Log disconnected keyboards
        for (kbd_id, meta) in &self.all_keyboards {
            if !meta.connected {
                info!("Keyboard disconnected: {} ({})", meta.name, kbd_id);
            }
        }
    }

    /// Synchronize keyboards to active users based on their configs
    fn sync_keyboards_to_users(&mut self) {
        // Load configs for active users (if not already loaded)
        self.load_user_configs();

        // First, stop processors for disconnected keyboards
        let disconnected_keyboards: Vec<_> = self
            .all_keyboards
            .iter()
            .filter(|(_, meta)| !meta.connected)
            .map(|(id, _)| id.clone())
            .collect();

        for kbd_id in disconnected_keyboards {
            info!("Stopping processors for disconnected keyboard: {}", kbd_id);
            self.stop_processors_for_keyboard(&kbd_id);
            self.keyboard_owners.remove(&kbd_id);
        }

        // Collect keyboard data first to avoid borrow checker issues
        let keyboards: Vec<_> = self
            .all_keyboards
            .iter()
            .filter(|(_, meta)| meta.connected) // Only process connected keyboards
            .map(|(id, meta)| (id.clone(), meta.clone()))
            .collect();

        // For each keyboard, check if any active user wants it
        for (kbd_id, meta) in keyboards {
            let mut assigned_uid = None;

            // Check existing ownership first
            if let Some(&owner_uid) = self.keyboard_owners.get(&kbd_id) {
                // Verify owner session is still active
                if self.session_manager.is_user_active(owner_uid) {
                    // Check if keyboard is still enabled in their config
                    if let Some(config_mgr) = self.user_configs.get(&owner_uid) {
                        let config = config_mgr.get_config();
                        let event_path = meta
                            .paths
                            .first()
                            .and_then(|p| p.file_name().and_then(|n| n.to_str()));
                        let enabled = config.is_keyboard_enabled(
                            &kbd_id.to_string(),
                            Some(&meta.name),
                            event_path,
                        );

                        if enabled {
                            assigned_uid = Some(owner_uid);
                        } else {
                            info!("User {} disabled keyboard {}", owner_uid, meta.name);
                        }
                    }
                } else {
                    info!(
                        "User {} session no longer active, releasing keyboard {}",
                        owner_uid, kbd_id
                    );
                }
            }

            // If not assigned, check other active users (first-come-first-serve)
            if assigned_uid.is_none() {
                let user_configs: Vec<_> = self
                    .user_configs
                    .iter()
                    .map(|(uid, cfg)| (*uid, cfg.clone()))
                    .collect();
                for (uid, config_mgr) in user_configs {
                    if !self.session_manager.is_user_active(uid) {
                        continue;
                    }

                    let config = config_mgr.get_config();
                    let event_path = meta
                        .paths
                        .first()
                        .and_then(|p| p.file_name().and_then(|n| n.to_str()));
                    let wants_keyboard = config.is_keyboard_enabled(
                        &kbd_id.to_string(),
                        Some(&meta.name),
                        event_path,
                    );

                    if wants_keyboard {
                        info!("Assigning keyboard {} to user {}", meta.name, uid);
                        assigned_uid = Some(uid);
                        break;
                    }
                }
            }

            // Start or stop processor based on assignment
            if let Some(uid) = assigned_uid {
                // Check if already running for this user by checking if ANY event path for this keyboard is active
                let has_active_processors = meta.paths.iter().any(|path| {
                    self.active_processors
                        .get(path)
                        .map(|(_, owner_uid, _)| *owner_uid == uid)
                        .unwrap_or(false)
                });

                if !has_active_processors {
                    // Stop any existing processors for this keyboard (might be owned by different user)
                    self.stop_processors_for_keyboard(&kbd_id);

                    // Start ONE THREAD PER EVENT FILE
                    if let Err(e) = self
                        .start_processors_for_keyboard(&kbd_id, &meta.name, &meta.paths, uid)
                                            {
                        error!("Failed to start processors for user {}: {}", uid, e);
                    } else {
                        self.keyboard_owners.insert(kbd_id.clone(), uid);
                    }
                }
            } else {
                // No user wants this keyboard, stop if running
                let has_processors = meta
                    .paths
                    .iter()
                    .any(|path| self.active_processors.contains_key(path));

                if has_processors {
                    info!(
                        "No active user wants keyboard {}, stopping processors",
                        meta.name
                    );
                    self.stop_processors_for_keyboard(&kbd_id);
                    self.keyboard_owners.remove(&kbd_id);
                }
            }
        }
    }

    /// Load configs for all active users
    fn load_user_configs(&mut self) {
        // Get active user UIDs (session state already refreshed by caller)
        let active_uids = self.get_active_user_uids();
        debug!("Active user UIDs: {:?}", active_uids);

        for &uid in &active_uids {
            // Skip if already loaded
            if self.user_configs.contains_key(&uid) {
                continue;
            }

            // Get user's home directory
            let home_dir = match crate::get_user_home_dir(uid) {
                Ok(dir) => dir,
                Err(e) => {
                    warn!("Failed to get home directory for user {}: {}", uid, e);
                    continue;
                }
            };

            let config_path = home_dir.join(".config/keymux/config.ron");

            // Load user's config
            match ConfigManager::new(config_path.clone()) {
                Ok(config_mgr) => {
                    info!("Loaded config for user {} from {:?}", uid, config_path);
                    self.user_configs.insert(uid, config_mgr);
                    self.warned_missing_config.remove(&uid);
                }
                Err(e) => {
                    // Warned once per session (not every 5s refresh tick) so
                    // this is actually visible in `journalctl -u keymux`
                    // under the default log level, instead of the daemon
                    // silently doing nothing for this user forever.
                    if self.warned_missing_config.insert(uid) {
                        warn!(
                            "No config for user {} at {:?}: {} - run `keymux init` as that user",
                            uid, config_path, e
                        );
                    }
                }
            }
        }

        // Remove configs for inactive users
        self.user_configs.retain(|uid, _| active_uids.contains(uid));
        self.warned_missing_config
            .retain(|uid| active_uids.contains(uid));
    }

    /// Get list of active user UIDs
    fn get_active_user_uids(&self) -> Vec<u32> {
        self.session_manager.get_active_uids()    }

    /// Get username from UID
    fn get_username(&self, uid: u32) -> Result<String> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(format!("getent passwd {} | cut -d: -f1", uid))
            .output()?;

        if !output.status.success() {
            return Err(anyhow::anyhow!("Failed to get username for UID {}", uid));
        }

        let username = String::from_utf8(output.stdout)?.trim().to_string();

        if username.is_empty() {
            return Err(anyhow::anyhow!("Empty username for UID {}", uid));
        }

        Ok(username)
    }


    /// Send desktop notification to a user
    fn send_notification(&self, uid: u32, title: &str, message: &str, urgency: &str) {
        info!("Attempting to send notification to user {}: {}", uid, title);

        // Get username from UID
        let username = match self.get_username(uid) {
            Ok(name) => name,
            Err(e) => {
                error!("Failed to get username for UID {}: {}", uid, e);
                return;
            }
        };

        info!("Resolved UID {} to username: {}", uid, username);

        match Command::new("runuser")
            .args([
                "-u",
                &username,
                "--",
                "/usr/bin/notify-send",
                "-u",
                urgency,
                title,
                message,
            ])
            .spawn()
        {
            Ok(_) => info!(
                "Sent {} notification to user {} ({}): {}",
                urgency, username, uid, title
            ),
            Err(e) => error!(
                "Failed to send notification to user {} ({}): {}",
                username, uid, e
            ),
        }
    }

    /// Start event processors for ALL event files of a keyboard - ONE THREAD PER EVENT FILE!
    fn start_processors_for_keyboard(
        &mut self,
        kbd_id: &KeyboardId,
        kbd_name: &str,
        event_paths: &[PathBuf],
        uid: u32,
    ) -> Result<()> {
        // Get user's config and apply per-keyboard overrides
        let base_config = self
            .user_configs
            .get(&uid)
            .context("User config not loaded")?
            .get_config()
            ;

        // Get config path for command execution
        let config_path = self
            .user_configs
            .get(&uid)
            .context("User config not loaded")?
            .get_config_path();

        // Apply per-keyboard config overrides
        let config = base_config.for_keyboard(&kbd_id.to_string());

        info!(
            "Starting {} event processor thread(s) for: {} (user: {})",
            event_paths.len(),
            kbd_name,
            uid
        );

        // One keymap per physical keyboard. A keyboard can expose several event files,
        // and a chord can span them, so every file's thread shares this layer, chord,
        // and steno state.
        let mut keymap = event_processor::KeymapProcessor::new(&config, config_path, uid);
        let _ = keymap.load_adaptive_stats(uid); // Ignore errors if file doesn't exist
        let shared_keymap = Arc::new(Mutex::new(keymap));

        // Track which paths we successfully started so we can roll back on partial failure
        let mut started_paths: Vec<PathBuf> = Vec::new();

        // Spawn ONE THREAD PER EVENT FILE
        for (idx, event_path) in event_paths.iter().enumerate() {
            // Check if already running
            if self.active_processors.contains_key(event_path) {
                warn!("Processor already running for: {}", event_path.display());
                continue;
            }

            // Open device — on failure, roll back any processors already started this call
            let device = match Device::open(event_path)
                .with_context(|| format!("Failed to open device: {}", event_path.display()))
            {
                Ok(d) => d,
                Err(e) => {
                    // Shut down any processors we already started in this call
                    for path in &started_paths {
                        if let Some((_, _, mut handle)) = self.active_processors.remove(path) {
                            let _ = handle.shutdown_tx.send(());
                            if let Some(th) = handle.thread_handle.take() {
                                let _ = th.join();
                            }
                        }
                    }
                    return Err(e);
                }
            };

            // Create channels
            let (shutdown_tx, shutdown_rx) = crossbeam_channel::bounded(1);
            let (game_mode_tx, game_mode_rx) = mpsc::channel();
            let (save_stats_tx, save_stats_rx) = mpsc::channel();

            // Spawn ONE real thread per event file — no wrapper, the JoinHandle
            // tracks the actual processor loop.  A clone of dead_tx is moved into
            // the thread; when the thread exits (ENODEV, shutdown, or error) the
            // send fires and the daemon's select! arm cleans up immediately.
            let kbd_id_clone = kbd_id.clone();
            let kbd_name_clone = kbd_name.to_string();
            let event_path_clone = event_path.clone();
            let shared_keymap_clone = Arc::clone(&shared_keymap);
            let dead_tx = self.processor_dead_tx.clone();

            let handle = thread::spawn(move || {
                info!(
                    "Event processor thread started for {} (event file: {})",
                    kbd_name_clone,
                    event_path_clone.display()
                );
                event_processor::run_processor(
                    kbd_id_clone,
                    device,
                    kbd_name_clone,
                    shared_keymap_clone,
                    uid,
                    shutdown_rx,
                    game_mode_rx,
                    save_stats_rx,
                );
                // Notify daemon that this processor is gone
                let _ = dead_tx.send(event_path_clone);
            });

            // Store processor handle indexed by EVENT PATH
            self.active_processors.insert(
                event_path.clone(),
                (
                    kbd_id.clone(),
                    uid,
                    ProcessorHandle {
                        shutdown_tx,
                        game_mode_tx: game_mode_tx.clone(),
                        save_stats_tx: save_stats_tx.clone(),
                        thread_handle: Some(handle),
                    },
                ),
            );

            // Track this path for rollback purposes
            started_paths.push(event_path.clone());

            // Send current game mode state to the new thread to preserve state across restarts
            let _ = game_mode_tx.send(self.game_mode_active);

            info!(
                "Started thread {}/{} for {} at {} (game_mode: {})",
                idx + 1,
                event_paths.len(),
                kbd_name,
                event_path.display(),
                self.game_mode_active
            );
        }

        Ok(())
    }

    /// Stop ALL event processors for a keyboard
    fn stop_processors_for_keyboard(&mut self, kbd_id: &KeyboardId) {
        // Find all event paths for this keyboard
        let paths_to_stop: Vec<PathBuf> = self
            .active_processors
            .iter()
            .filter(|(_, (k_id, _, _))| k_id == kbd_id)
            .map(|(path, _)| path.clone())
            .collect();

        if paths_to_stop.is_empty() {
            return;
        }

        info!(
            "Stopping {} processor thread(s) for: {}",
            paths_to_stop.len(),
            kbd_id
        );

        // Collect thread handles after sending all shutdown signals, then await them.
        // This ensures the old processor has fully released its device grab before we
        // attempt to open and re-grab the device for the new processor on reconnect.
        let mut join_tasks = Vec::new();

        for path in paths_to_stop {
            if let Some((_, _, mut handle)) = self.active_processors.remove(&path) {
                // Send shutdown signal
                let _ = handle.shutdown_tx.send(());

                if let Some(thread_handle) = handle.thread_handle.take() {
                    // Await the thread with a generous timeout so we don't block forever
                    // on a stuck thread, but DO wait long enough for the ungrab to complete.
                    let path_str = path.display().to_string();
                    join_tasks.push((path_str, thread_handle));
                } else {
                    info!("Stopped processor for: {}", path.display());
                }
            }
        }

        // Every thread was told to stop before any is waited for, so this takes as long as
        // the slowest of them, not the sum
        for (path_str, thread_handle) in join_tasks {
            if thread_handle.join().is_err() {
                warn!("Processor thread panicked: {}", path_str);
            }
            info!("Stopped processor for: {}", path_str);
        }
    }

    /// Start hotplug monitor (udev)
    fn start_hotplug_monitor(&self) -> Receiver<String> {
        let (tx, rx) = unbounded();

        thread::spawn(move || {
            loop {
                // Use udevadm to monitor for input device changes
                let mut child = Command::new("udevadm")
                    .arg("monitor")
                    .arg("--udev")
                    .arg("--subsystem-match=input")
                    .stdout(Stdio::piped())
                    .spawn()
                    .expect("Failed to start udevadm monitor");

                if let Some(stdout) = child.stdout.take() {
                    use std::io::BufRead;
                    let reader = std::io::BufReader::new(stdout);

                    for line in reader.lines().map_while(Result::ok) {
                        if line.contains("event") {
                            let _ = tx.send(line);
                        }
                    }
                }

                // Wait for child process to exit to avoid zombies
                let _ = child.wait();

                // If udevadm exits, restart it
                warn!("udevadm monitor died, restarting...");
                thread::sleep(Duration::from_secs(1));
            }
        });

        rx
    }

    /// Start IPC server
    fn start_ipc_server(
        &self,
    ) -> Result<Receiver<(IpcRequest, mpsc::Sender<IpcResponse>)>> {
        let (tx, rx) = unbounded();
        let socket_path = get_root_socket_path();

        // Remove old socket if exists
        let _ = std::fs::remove_file(&socket_path);

        // Create socket directory
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let listener = UnixListener::bind(&socket_path).context("Failed to bind IPC socket")?;

        // Set socket permissions to allow user access (mode 0666)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let permissions = std::fs::Permissions::from_mode(0o666);
            if let Err(e) = std::fs::set_permissions(&socket_path, permissions) {
                warn!("Failed to set socket permissions: {}", e);
            } else {
                info!("Socket permissions set to 0666 (world-readable/writable)");
            }
        }

        info!("IPC server listening on: {:?}", socket_path);

        thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(mut stream) => {
                        use std::io::{Read, Write};

                        // Read length prefix (4 bytes)
                        let mut len_buf = [0u8; 4];
                        if let Err(e) = stream.read_exact(&mut len_buf) {
                            error!("Failed to read IPC length: {}", e);
                            continue;
                        }
                        let len = u32::from_le_bytes(len_buf) as usize;

                        // Read request data
                        let mut buffer = vec![0u8; len];
                        match stream.read_exact(&mut buffer) {
                            Ok(()) => {
                                if let Ok(request) = bincode::deserialize::<IpcRequest>(&buffer) {
                                    // A subscription keeps its connection, so it gets its own
                                    // thread and this loop goes back to accepting
                                    if matches!(request, IpcRequest::SubscribeOverlay) {
                                        thread::spawn(move || {
                                            crate::ipc::serve_overlay(
                                                stream,
                                                crate::steno::overlay::OverlayHub::global(),
                                            );
                                        });
                                        continue;
                                    }
                                    // Create response channel
                                    let (resp_tx, resp_rx) = mpsc::channel();

                                    // Send to main loop
                                    if tx.send((request, resp_tx)).is_ok() {
                                        // Wait for response
                                        if let Ok(response) =
                                            resp_rx.recv_timeout(Duration::from_secs(5))
                                        {
                                            if let Ok(resp_bytes) = bincode::serialize(&response) {
                                                // Send length prefix
                                                let resp_len =
                                                    (resp_bytes.len() as u32).to_le_bytes();
                                                let _ = stream.write_all(&resp_len);
                                                // Send response data
                                                let _ = stream.write_all(&resp_bytes);
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => error!("Failed to read IPC request: {}", e),
                        }
                    }
                    Err(e) => error!("Failed to accept IPC connection: {}", e),
                }
            }
        });

        Ok(rx)
    }

    /// Start niri window monitor
    fn start_niri_monitor(
        &self,
    ) -> Receiver<crate::window_manager::WindowManagerEvent> {
        if !crate::niri::is_niri_available() {
            debug!("Niri not available, skipping window monitor");
            return never();
        }
        let (tx, rx) = unbounded();
        crate::niri::start_niri_monitor_sync(tx);
        info!("Started niri window monitor");
        rx
    }

    /// Start config file watcher for automatic reload
    /// Returns: Receiver<()> that signals when any config changed
    fn start_config_watcher(&self) -> Receiver<()> {
        use notify::{recommended_watcher, Event, EventKind, RecursiveMode, Watcher};

        let (tx, rx) = unbounded();

        thread::spawn(move || {
            let (watch_tx, watch_rx) = std::sync::mpsc::channel();

            let mut watcher: Box<dyn Watcher> = match recommended_watcher(watch_tx) {
                Ok(w) => Box::new(w),
                Err(e) => {
                    error!("Failed to create config file watcher: {}", e);
                    return;
                }
            };

            // Track both original paths and resolved targets for symlinks
            let mut watched_paths: HashSet<PathBuf> = HashSet::new();
            let mut watched_dirs: HashSet<PathBuf> = HashSet::new();

            /// Resolve symlinks to get the final target path
            #[allow(clippy::option_if_let_else)]
            fn resolve_symlink(path: &Path) -> Option<PathBuf> {
                match std::fs::symlink_metadata(path) {
                    Ok(metadata) => {
                        if metadata.file_type().is_symlink() {
                            match std::fs::read_link(path) {
                                Ok(link_target) => {
                                    // If the symlink target is relative, resolve it relative to the symlink's parent
                                    let resolved = if link_target.is_absolute() {
                                        link_target
                                    } else {
                                        path.parent()
                                            .unwrap_or_else(|| Path::new("."))
                                            .join(&link_target)
                                            .canonicalize()
                                            .unwrap_or(link_target)
                                    };
                                    Some(resolved)
                                }
                                Err(_) => None,
                            }
                        } else {
                            Some(path.to_path_buf())
                        }
                    }
                    Err(_) => None,
                }
            }

            /// Add a config to be watched, handling symlinks properly
            fn add_config_watch(
                config_path: PathBuf,
                watcher: &mut Box<dyn Watcher>,
                watched_paths: &mut HashSet<PathBuf>,
                watched_dirs: &mut HashSet<PathBuf>,
            ) {
                // Watch the directory containing the config
                let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));

                // Resolve any symlinks in the directory path
                let Some(resolved_dir) = resolve_symlink(config_dir) else {
                    warn!("Failed to resolve config directory: {:?}", config_dir);
                    return;
                };

                // Watch the resolved directory
                if let Err(e) = watcher.watch(&resolved_dir, RecursiveMode::NonRecursive) {
                    warn!("Failed to watch directory {:?}: {}", resolved_dir, e);
                    return;
                }

                // Track both original and resolved paths
                watched_paths.insert(config_path.clone());
                watched_dirs.insert(resolved_dir.clone());

                // If config path itself is a symlink, also watch its resolved target
                if let Some(resolved_config) = resolve_symlink(&config_path) {
                    if resolved_config != config_path {
                        let resolved_config_dir =
                            resolved_config.parent().unwrap_or_else(|| Path::new("."));
                        if resolved_config_dir != resolved_dir {
                            if let Err(e) =
                                watcher.watch(resolved_config_dir, RecursiveMode::NonRecursive)
                            {
                                warn!(
                                    "Failed to watch symlink target directory {:?}: {}",
                                    resolved_config_dir, e
                                );
                            } else {
                                watched_dirs.insert(resolved_config_dir.to_path_buf());
                            }
                        }
                        watched_paths.insert(resolved_config);
                    }
                }

                let symlink_info = std::fs::symlink_metadata(&config_path)
                    .map(|metadata| {
                        if metadata.file_type().is_symlink() {
                            format!(
                                " (symlink -> {})",
                                resolve_symlink(&config_path).unwrap_or_default().display()
                            )
                        } else {
                            String::new()
                        }
                    })
                    .unwrap_or_default();

                info!(
                    "Watching config at {}{}",
                    config_path.display(),
                    symlink_info
                );
            }

            // Scan for users with keymux configs
            if let Ok(entries) = std::fs::read_dir("/home") {
                for entry in entries.flatten() {
                    let home_dir = entry.path();
                    let config_dir = home_dir.join(".config/keymux");
                    let config_path = config_dir.join("config.ron");

                    if config_path.exists() {
                        add_config_watch(
                            config_path,
                            &mut watcher,
                            &mut watched_paths,
                            &mut watched_dirs,
                        );
                    }
                }
            }

            info!(
                "Config file watcher started for {} config(s) in {} director(y/ies)",
                watched_paths.len(),
                watched_dirs.len()
            );

            loop {
                match watch_rx.recv() {
                    Ok(Ok(Event {
                        kind: EventKind::Modify(_) | EventKind::Create(_),
                        paths,
                        ..
                    })) => {
                        // Check if any modified file relates to our watched configs
                        let mut detected_change = false;
                        for path in paths {
                            // Check direct path match
                            if watched_paths.contains(&path) {
                                info!("Config file changed: {:?}", path);
                                detected_change = true;
                                break;
                            }

                            // Check if this path is a symlink target of any watched config
                            for watched_path in &watched_paths {
                                if let Some(resolved) = resolve_symlink(watched_path) {
                                    if path == resolved {
                                        info!("Config file changed via symlink target: {:?} (original: {:?})", path, watched_path);
                                        detected_change = true;
                                        break;
                                    }
                                }
                            }

                            if detected_change {
                                break;
                            }
                        }

                        if detected_change {
                            // Debounce: drain all events for the next 300ms
                            let debounce_start = std::time::Instant::now();
                            while debounce_start.elapsed() < Duration::from_millis(300) {
                                if watch_rx.recv_timeout(Duration::from_millis(50)).is_err() {
                                    break;
                                }
                                // Drain event, continue debouncing
                            }

                            // Send single reload signal after debounce
                            info!("Config changes settled, triggering reload");
                            let _ = tx.send(());
                        }
                    }
                    Ok(Ok(_)) => {} // Ignore other event types
                    Ok(Err(e)) => error!("Config watch error: {}", e),
                    Err(e) => {
                        error!("Config watch channel error: {}", e);
                        break;
                    }
                }
            }
        });

        rx
    }

    /// Whether any loaded config differs, in how it behaves, from the file on disk now. A file
    /// that can't be read or parsed counts as changed, so the reload reports what is wrong.
    fn config_changed_on_disk(&self) -> bool {
        // Nothing loaded yet, or a user who has no config loaded: a reload may load one
        if self.user_configs.is_empty() {
            return true;
        }
        for mgr in self.user_configs.values() {
            let Ok(on_disk) = crate::config::Config::load(&mgr.get_config_path()) else {
                return true;
            };
            if !mgr.get_config().behaves_like(&on_disk) {
                return true;
            }
        }
        false
    }

    /// Reload all user configs and restart processors
    fn reload_all_configs(&mut self) -> Result<()> {
        info!("Reloading all user configs...");
        self.refresh_sessions();

        // Step 1: Validate all configs before stopping anything
        info!("Validating configs...");
        let active_uids = self.get_active_user_uids();
        debug!("Active UIDs for validation: {:?}", active_uids);
        let mut validation_errors: HashMap<u32, String> = HashMap::new();

        for &uid in &active_uids {
            let Ok(home_dir) = crate::get_user_home_dir(uid) else {
                continue;
            };

            let config_path = home_dir.join(".config/keymux/config.ron");
            if config_path.exists() {
                // Try to load and validate
                let new_config = match crate::config::Config::load(&config_path) {
                    Ok(cfg) => cfg,
                    Err(e) => {
                        error!("Config load failed for user {}: {}", uid, e);
                        let error_msg = format!("Config load failed: {}", e);
                        validation_errors.insert(uid, error_msg);
                        continue;
                    }
                };
                if let Err(e) = new_config.validate_silent() {
                    error!("Config validation failed for user {}: {}", uid, e);
                    let error_msg = format!("Config validation failed: {}", e);
                    validation_errors.insert(uid, error_msg);
                }
            }
        }

        // If any configs failed validation, send error notifications and abort reload
        if !validation_errors.is_empty() {
            info!(
                "Sending error notifications to {} users",
                validation_errors.len()
            );

            for (uid, error_msg) in &validation_errors {
                info!("Sending error notification to user {}: {}", uid, error_msg);
                self.send_notification(
                    *uid,
                    "Keyboard Middleware - Config Error",
                    error_msg,
                    "critical",
                );
            }

            // Return first error for logging purposes
            let first_error = validation_errors.iter().next().unwrap();
            return Err(anyhow::anyhow!(
                "Invalid config for user {}: {}",
                first_error.0,
                first_error.1
            ));
        }

        // Step 2: Stop all processors and clear ownership state
        info!("Stopping all processors...");
        let all_kbd_ids: Vec<_> = self.keyboard_owners.keys().cloned().collect();
        for kbd_id in all_kbd_ids {
            self.stop_processors_for_keyboard(&kbd_id);
        }
        self.keyboard_owners.clear();

        // Step 3: Clear and reload configs
        info!("Reloading configs from disk...");
        self.user_configs.clear();
        self.load_user_configs();

        // Step 4: Restart all processors with new configs
        info!("Restarting processors with new configs...");
        self.sync_keyboards_to_users();

        info!("Config reload complete!");

        // Step 5: Send success notifications to users who own keyboards
        let owner_uids: HashSet<u32> = self.keyboard_owners.values().copied().collect();
        info!("Keyboard owners: {:?}", self.keyboard_owners);
        info!(
            "Sending success notifications to {} users: {:?}",
            owner_uids.len(),
            owner_uids
        );

        for uid in owner_uids {
            info!("Sending notification to user {}", uid);
            self.send_notification(
                uid,
                "Keyboard Middleware",
                "Configuration reloaded successfully!",
                "normal",
            );
        }

        Ok(())
    }

    /// Resolve the effective game mode for a window, applying the global
    /// override, then the per-app_id window override, then falling back to
    /// the programmed detection heuristics.
    fn resolve_effective_game_mode(
        &self,
        window: &WindowInfo,
    ) -> crate::niri::gamemode_detection::GameModeState {
        resolve_effective_game_mode(
            window.app_id.as_deref(),
            window.pid,
            window.title.as_deref(),
            self.global_override,
            &self.window_overrides,
        )
    }

    /// Record a window focus event and apply its resulting game mode.
    /// Called for every focus-change event reported by any watcher process.
    fn handle_window_focus_changed(&mut self, window: WindowInfo) {
        let state = self.resolve_effective_game_mode(&window);
        debug!(
            "Window focus changed: app_id={:?}, game mode={:?}",
            window.app_id, state
        );
        // Another window has a different caret and different text, so what steno remembers of
        // the old one is no longer what is before the cursor
        let moved = self
            .last_focused_window
            .as_ref()
            .is_none_or(|last| last.app_id != window.app_id || last.pid != window.pid);
        if moved {
            crate::steno::journal::fence_all();
        }
        self.last_focused_window = Some(window);
        self.set_game_mode_all(state.is_game_mode());
    }

    /// Recompute game mode from the last known focused window and apply it.
    /// Called whenever an override changes, so the effect is immediate
    /// rather than waiting for the next focus-change event.
    fn recompute_and_apply_game_mode(&mut self) {
        if let Some(window) = self.last_focused_window.clone() {
            let state = self.resolve_effective_game_mode(&window);
            self.set_game_mode_all(state.is_game_mode());
        }
    }

    /// Stop every active processor thread. Used for graceful shutdown.
    fn stop_all_processors(&mut self) {
        let paths: Vec<PathBuf> = self.active_processors.keys().cloned().collect();
        info!("Stopping {} processor thread(s) for shutdown", paths.len());

        let mut join_tasks = Vec::new();
        for path in paths {
            if let Some((_, _, mut handle)) = self.active_processors.remove(&path) {
                let _ = handle.shutdown_tx.send(());
                if let Some(thread_handle) = handle.thread_handle.take() {
                    join_tasks.push((path.display().to_string(), thread_handle));
                }
            }
        }

        for (path_str, thread_handle) in join_tasks {
            if thread_handle.join().is_err() {
                warn!("Processor thread panicked during shutdown: {}", path_str);
            }
            info!("Stopped processor for: {}", path_str);
        }
    }

    /// Handle a single IPC request
    fn handle_ipc_request(&mut self, request: IpcRequest) -> IpcResponse {
        match request {
            IpcRequest::Ping => IpcResponse::Pong,
            IpcRequest::WindowFocusChanged { app_id, pid, title } => {
                self.handle_window_focus_changed(WindowInfo { app_id, pid, title });
                IpcResponse::Ok
            }
            IpcRequest::GetFocusedWindow => {
                IpcResponse::FocusedWindow(self.last_focused_window.clone().unwrap_or(WindowInfo {
                    app_id: None,
                    pid: None,
                    title: None,
                }))
            }
            IpcRequest::SetWindowOverride { app_id, state } => {
                match state {
                    Some(s) => {
                        self.window_overrides.insert(app_id, s);
                    }
                    None => {
                        self.window_overrides.remove(&app_id);
                    }
                }
                self.recompute_and_apply_game_mode();
                IpcResponse::Ok
            }
            IpcRequest::ToggleWindowOverride { app_id } => {
                // Determine the currently-effective boolean for this app_id
                // (using pid/title context if it happens to be the window
                // we last saw focused), then flip it into an explicit
                // override.
                let (pid, title) = self
                    .last_focused_window
                    .as_ref()
                    .filter(|w| w.app_id.as_deref() == Some(app_id.as_str()))
                    .map(|w| (w.pid, w.title.clone()))
                    .unwrap_or((None, None));

                let currently_enabled = resolve_effective_game_mode(
                    Some(&app_id),
                    pid,
                    title.as_deref(),
                    self.global_override,
                    &self.window_overrides,
                )
                .is_game_mode();

                let new_state = if currently_enabled {
                    WindowOverride::Off
                } else {
                    WindowOverride::On
                };
                self.window_overrides.insert(app_id, new_state);
                self.recompute_and_apply_game_mode();
                IpcResponse::Ok
            }
            IpcRequest::ListWindowOverrides => {
                let mut overrides: Vec<(String, WindowOverride)> = self
                    .window_overrides
                    .iter()
                    .map(|(k, v)| (k.clone(), *v))
                    .collect();
                overrides.sort_by(|a, b| a.0.cmp(&b.0));
                IpcResponse::WindowOverrides(overrides)
            }
            IpcRequest::SetGlobalOverride(state) => {
                self.global_override = state;
                self.recompute_and_apply_game_mode();
                IpcResponse::Ok
            }
            IpcRequest::GetGlobalOverride => {
                IpcResponse::GlobalOverrideStatus(self.global_override)
            }
            IpcRequest::ListKeyboards => {
                // Collect all enabled_keyboards entries from all user configs for annotation
                let mut all_config_entries: Vec<(String, bool)> = Vec::new(); // (pattern, is_enable)
                for config_mgr in self.user_configs.values() {
                    let config = config_mgr.get_config();
                    if let Some(entries) = config.get_enabled_keyboards_entries() {
                        for entry in entries {
                            let pattern = entry.pattern().to_string();
                            let is_enable =
                                entry.action() == crate::config::config::EnableDisable::Enable;
                            all_config_entries.push((pattern, is_enable));
                        }
                    }
                }

                let keyboards = self
                    .all_keyboards
                    .iter()
                    .map(|(id, meta)| {
                        // Use first path as representative (for display)
                        let device_path = meta
                            .paths
                            .first()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default();

                        // Keyboard is enabled if ANY of its event paths have active processors
                        let enabled = meta
                            .paths
                            .iter()
                            .any(|path| self.active_processors.contains_key(path));

                        // Check if any matching config entry is portless (no '@')
                        // so the display can annotate the full id@port appropriately
                        let enabled_by_portless = enabled
                            && all_config_entries
                                .iter()
                                .any(|(e, _)| id.matches_config_entry(e) && !e.contains('@'));

                        // Find the matching rule that determined this keyboard's enabled state
                        let keyboard_id_str = id.to_string();
                        let keyboard_name = Some(meta.name.as_str());
                        let event_path = meta
                            .paths
                            .first()
                            .and_then(|p| p.file_name().and_then(|n| n.to_str()));

                        let mut matched_rule: Option<String> = None;
                        let mut last_action: Option<bool> = None;

                        // Apply matching rules (last match wins)
                        for (pattern, is_enable) in &all_config_entries {
                            // Check for glob "*"
                            if *pattern == "*" {
                                matched_rule = Some(pattern.clone());
                                last_action = Some(*is_enable);
                                continue;
                            }

                            // Check if this entry matches the keyboard
                            // Event path match (e.g., "event17"), or ID match or name match
                            let by_event = event_path.is_some_and(|event| pattern == event);
                            let by_id_or_name = keyboard_id_str.contains(pattern)
                                || keyboard_id_str.starts_with(pattern)
                                || keyboard_name.is_some_and(|name| name.contains(pattern));

                            if by_event || by_id_or_name {
                                matched_rule = Some(pattern.clone());
                                last_action = Some(*is_enable);
                            }
                        }

                        // With no matching rule the keyboard is implicitly enabled (default
                        // behavior, and backward compatible when there are rules but none match)

                        // Only set matched_rule if the keyboard's state matches the rule
                        // If it's implicit (no rule matched), set to None
                        let matched_rule = if last_action.is_some() {
                            matched_rule
                        } else {
                            None
                        };

                        crate::ipc::KeyboardInfo {
                            hardware_id: id.to_string(),
                            name: meta.name.clone(),
                            device_path,
                            enabled,
                            connected: meta.connected,
                            enabled_by_portless,
                            matched_rule,
                        }
                    })
                    .collect();
                IpcResponse::KeyboardList(keyboards)
            }
            IpcRequest::ToggleKeyboards => {
                info!("Toggle keyboards requested via IPC");
                match self.reload_all_configs() {
                    Ok(()) => IpcResponse::Ok,
                    Err(e) => {
                        error!("Toggle reload failed: {}", e);
                        IpcResponse::Error(format!("Toggle failed: {}", e))
                    }
                }
            }
            IpcRequest::EnableKeyboard(hardware_id) => {
                info!("Enable keyboard requested via IPC: {}", hardware_id);
                // Create KeyboardId from string
                let kbd_id = crate::keyboard_id::KeyboardId::new(hardware_id.clone());
                // Check if keyboard exists
                if self.all_keyboards.contains_key(&kbd_id) {
                    // Find the first active user to assign to
                    // In multi-user mode, we need to know which user's config to update
                    // For now, we'll trigger a resync which will check all user configs
                    info!("Keyboard {} found, triggering resync", hardware_id);
                    self.sync_keyboards_to_users();
                    IpcResponse::Ok
                } else {
                    IpcResponse::Error(format!("Keyboard not found: {}", hardware_id))
                }
            }
            IpcRequest::DisableKeyboard(hardware_id) => {
                info!("Disable keyboard requested via IPC: {}", hardware_id);
                let kbd_id = crate::keyboard_id::KeyboardId::new(hardware_id);
                self.stop_processors_for_keyboard(&kbd_id);
                self.keyboard_owners.remove(&kbd_id);
                IpcResponse::Ok
            }
            IpcRequest::Reload => {
                info!("Config reload requested via IPC");
                match self.reload_all_configs() {
                    Ok(()) => IpcResponse::Ok,
                    Err(e) => {
                        error!("Config reload failed: {}", e);
                        IpcResponse::Error(format!("Reload failed: {}", e))
                    }
                }
            }
            IpcRequest::SaveAdaptiveStats => {
                info!("Save adaptive stats requested via IPC");
                self.save_adaptive_stats_all();
                IpcResponse::Ok
            }
            IpcRequest::Shutdown => {
                info!("Shutdown requested via IPC");
                self.stop_all_processors();
                // Exit shortly after returning, so the client still gets its
                // `Ok` response before the process disappears.
                thread::spawn(|| {
                    thread::sleep(Duration::from_millis(100));
                    info!("Daemon exiting after graceful shutdown");
                    std::process::exit(0);
                });
                IpcResponse::Ok
            }
            // The IPC thread hands subscriptions to their own thread before they get here
            IpcRequest::SubscribeOverlay => {
                IpcResponse::Error("overlay subscriptions are streamed, not answered".into())
            }
        }
    }

    /// Process a single window-focus event from the daemon's own inline
    /// niri monitor (`start_niri_monitor`). Note: this only matters when the
    /// root daemon itself can see a niri socket, which in the common
    /// multi-user deployment (root daemon, user's niri session) it normally
    /// cannot - the standalone `keymux niri-daemon` watcher process
    /// (running as the user, reporting via `WindowFocusChanged` IPC) is what
    /// actually drives game mode detection in that setup. Both paths route
    /// through the same `handle_window_focus_changed` evaluator.
    fn process_niri_event(&mut self, event: crate::window_manager::WindowManagerEvent) {
        match event {
            crate::window_manager::WindowManagerEvent::WindowFocusChanged(window_info) => {
                self.handle_window_focus_changed(window_info);
            }
        }
    }

    /// Set game mode for all active processors
    fn set_game_mode_all(&mut self, enabled: bool) {
        // Only update if the state actually changed
        if self.game_mode_active == enabled {
            return;
        }

        info!(
            "Setting game mode to: {} ({} active threads)",
            enabled,
            self.active_processors.len()
        );

        // Store the new state so new threads will get it
        self.game_mode_active = enabled;

        // Send to all active threads
        for (_, _, handle) in self.active_processors.values() {
            let _ = handle.game_mode_tx.send(enabled);
        }
    }

    /// Trigger adaptive stats save for all active processors
    fn save_adaptive_stats_all(&self) {
        info!(
            "Triggering adaptive stats save for {} active threads",
            self.active_processors.len()
        );

        // Send save signal to all active threads
        for (_, _, handle) in self.active_processors.values() {
            let _ = handle.save_stats_tx.send(());
        }
    }

    /// Refresh user sessions
    fn refresh_sessions(&mut self) {
        if let Err(e) = self.session_manager.refresh_sessions() {
            error!("Failed to refresh sessions: {}", e);
        }
    }

    /// The config file was written. It is reloaded only if some user asked for hot reload,
    /// and only if the save changed how the config behaves: a comment, spacing or a rewrite
    /// of the same text must not restart every keyboard.
    fn config_file_changed(&mut self) {
        let hot_reload_enabled = self
            .user_configs
            .values()
            .any(|mgr| mgr.config().hot_config_reload);
        if !hot_reload_enabled {
            return;
        }
        if self.config_changed_on_disk() {
            info!("Config file changed, reloading...");
            if let Err(e) = self.reload_all_configs() {
                error!("Config reload failed: {}", e);
            }
        } else {
            debug!("Config file saved with no change to how it behaves, not reloading");
        }
    }

    /// A processor thread died (ENODEV or an error): clean up now rather than when a udev
    /// event next triggers rediscovery.
    fn processor_died(&mut self, dead_path: &Path) {
        let Some((kbd_id, _, _)) = self.active_processors.remove(dead_path) else {
            return;
        };
        info!(
            "Processor thread died for: {} ({})",
            dead_path.display(),
            kbd_id
        );
        let any_remaining = self
            .active_processors
            .values()
            .any(|(k, _, _)| k == &kbd_id);
        if !any_remaining {
            info!("All processors dead for {}, marking disconnected", kbd_id);
            if let Some(meta) = self.all_keyboards.get_mut(&kbd_id) {
                meta.connected = false;
            }
            self.keyboard_owners.remove(&kbd_id);
        }
    }
}
