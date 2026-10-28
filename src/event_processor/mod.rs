use crate::keyboard_id::KeyboardId;
use crate::keycode::KeyCode;
use crate::steno::translate::OutputOp;
use actions::ProcessResult as ProcResult;
pub use actions::ProcessResult;
use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, Device, EventType, InputEvent, Key};
pub use keymap::KeymapProcessor;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

pub mod actions;
pub mod adaptive;
pub mod keymap;
pub mod layer_stack;
#[cfg(test)]
mod steno_tests;

// SYN event constants
const SYN_REPORT: i32 = 0;
const SYN_CODE: u16 = 0;

/// Run the event processor loop for a single keyboard event file.
///
/// This is called directly inside the thread spawned by the daemon.
/// It blocks until the keyboard is unplugged (ENODEV), a shutdown signal
/// is received, or an unrecoverable error occurs.
#[allow(clippy::too_many_arguments)]
pub fn run_processor(
    keyboard_id: KeyboardId,
    mut device: Device,
    keyboard_name: String,
    keymap: Arc<Mutex<KeymapProcessor>>,
    user_id: u32,
    shutdown_rx: crossbeam_channel::Receiver<()>,
    game_mode_rx: std::sync::mpsc::Receiver<bool>,
    save_stats_rx: std::sync::mpsc::Receiver<()>,
) {
    if let Err(e) = run_event_processor(
        &keyboard_id,
        &mut device,
        &keyboard_name,
        &keymap,
        user_id,
        shutdown_rx,
        game_mode_rx,
        save_stats_rx,
    ) {
        error!("Event processor for {} failed: {}", keyboard_id, e);
    }
    info!("Event processor thread exiting for: {}", keyboard_id);
}

#[allow(clippy::too_many_arguments)]
fn run_event_processor(
    keyboard_id: &KeyboardId,
    device: &mut Device,
    keyboard_name: &str,
    shared_keymap: &Arc<Mutex<KeymapProcessor>>,
    user_id: u32,
    shutdown_rx: crossbeam_channel::Receiver<()>,
    game_mode_rx: std::sync::mpsc::Receiver<bool>,
    save_stats_rx: std::sync::mpsc::Receiver<()>,
) -> Result<()> {
    info!(
        "Starting event processor for: {} ({})",
        keyboard_name, keyboard_id
    );

    // Set device to non-blocking mode so we can check shutdown signal
    let fd = device.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    // Grab the device for exclusive access
    device.grab().context("Failed to grab device")?;
    info!("Grabbed device: {}", keyboard_name);

    // Create virtual uinput device
    let mut virtual_device = create_virtual_device(device, keyboard_name)?;
    info!("Created virtual device for: {}", keyboard_name);
    let mut pacer = Pacer::default();

    // SAFETY: Release all keys immediately on startup to prevent stuck keys
    // This fixes the hotplug bug where keys remain held after reconnection
    release_all_keys_on_startup(&mut virtual_device);
    info!("Released all keys on startup for safety: {}", keyboard_name);

    // Track last save time for periodic stats saving
    let mut alt_f12 = AltF12::default();
    let mut last_stats_save = std::time::Instant::now();
    const STATS_SAVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

    // Event processing loop. Each pass locks the keyboard's shared keymap, so the
    // device threads for one keyboard take turns and see one chord and layer state.
    loop {
        let mut keymap = shared_keymap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Check for shutdown signal (non-blocking)
        match shutdown_rx.try_recv() {
            Ok(()) => {
                warn!("Shutdown signal received for: {}", keyboard_name);
                // Save adaptive timing stats before shutdown
                let _ = keymap.save_adaptive_stats(user_id);
                // Release all held keys before exiting (graceful shutdown)
                release_all_keys(&mut virtual_device, &keymap);
                // Ungrab device before exiting
                let _ = device.ungrab();
                info!("Device ungrabbed and released for: {}", keyboard_name);
                return Ok(());
            }
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                warn!("Shutdown channel disconnected for: {}", keyboard_name);
                // Release all held keys before exiting (graceful shutdown)
                release_all_keys(&mut virtual_device, &keymap);
                let _ = device.ungrab();
                return Ok(());
            }
            Err(crossbeam_channel::TryRecvError::Empty) => {
                // No shutdown signal, continue
            }
        }

        // Check for game mode toggle (non-blocking)
        match game_mode_rx.try_recv() {
            Ok(active) => {
                info!(
                    "Game mode {} for: {}",
                    if active { "enabled" } else { "disabled" },
                    keyboard_name
                );
                keymap.set_game_mode(active);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // No game mode toggle, continue
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Game mode channel disconnected, just log and continue
                debug!("Game mode channel disconnected for: {}", keyboard_name);
            }
        }

        // Check for save stats request (non-blocking)
        match save_stats_rx.try_recv() {
            Ok(()) => {
                info!("Save stats requested for: {}", keyboard_name);
                let _ = keymap.save_adaptive_stats(user_id);
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // No save request, continue
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Save stats channel disconnected, just log and continue
                debug!("Save stats channel disconnected for: {}", keyboard_name);
            }
        }

        // Periodically save adaptive timing stats
        if last_stats_save.elapsed() >= STATS_SAVE_INTERVAL {
            let _ = keymap.save_adaptive_stats(user_id);
            last_stats_save = std::time::Instant::now();
        }

        // Read events from physical keyboard (non-blocking)
        let mut escaped = false;
        match device.fetch_events() {
            Ok(events) => {
                for ev in events {
                    // Alt+F12 releases this keyboard from keymux. It's checked on the raw key,
                    // before the keymap, so no layer or remap can swallow it.
                    if ev.event_type() == EventType::KEY && alt_f12.feed(Key::new(ev.code()), ev.value()) {
                        // Let go of anything keymux is holding down before the keyboard is given back
                        warn!("Alt+F12 pressed on {}; releasing it from keymux", keyboard_name);
                        let _ = keymap.save_adaptive_stats(user_id);
                        release_all_keys(&mut virtual_device, &keymap);
                        escaped = true;
                        break;
                    }

                    // Process key events through keymap
                    if ev.event_type() == evdev::EventType::KEY {
                        // Convert evdev key code to our KeyCode enum
                        if let Some(input_key) = KeyCode::from_evdev_code(ev.code()) {
                            let pressed = ev.value() == 1; // 1 = press, 0 = release, 2 = repeat
                            let repeat = ev.value() == 2;

                            // Ignore repeat events
                            if repeat {
                                continue;
                            }

                            // Process key through keymap (QMK-inspired)
                            let result = keymap.process_key(input_key, pressed);

                            match result {
                                ProcessResult::EmitKey(output_key, output_pressed) => {
                                    // Convert back to evdev and emit
                                    let output_evdev = Key::new(output_key.code());
                                    let output_event = InputEvent::new_now(
                                        ev.event_type(),
                                        output_evdev.code(),
                                        i32::from(output_pressed),
                                    );
                                    virtual_device.emit(&[output_event])?;
                                }
                                ProcessResult::Retype { backspaces, text } => {
                                    retype(&mut virtual_device, &mut pacer, backspaces, &text)?;
                                }
                                ProcessResult::Ops(ops) => {
                                    emit_ops(&mut virtual_device, &mut pacer, &ops)?;
                                }
                                ProcessResult::TypeString(text, add_enter) => {
                                    // Type out the string character by character
                                    type_string(&mut virtual_device, &mut pacer, &text, add_enter)?;
                                }
                                ProcessResult::TapKeyPressRelease(tap_key) => {
                                    // Emit tap key press and release
                                    let key_evdev = Key::new(tap_key.code());
                                    let press_event =
                                        InputEvent::new_now(ev.event_type(), key_evdev.code(), 1);
                                    virtual_device.emit(&[press_event])?;

                                    std::thread::sleep(std::time::Duration::from_millis(5));

                                    let release_event =
                                        InputEvent::new_now(ev.event_type(), key_evdev.code(), 0);
                                    virtual_device.emit(&[release_event])?;
                                }
                                ProcessResult::MultipleEvents(events) => {
                                    // Emit multiple events in sequence
                                    for (key, pressed) in events {
                                        let key_evdev = Key::new(key.code());
                                        let event = InputEvent::new_now(
                                            ev.event_type(),
                                            key_evdev.code(),
                                            i32::from(pressed),
                                        );
                                        virtual_device.emit(&[event])?;
                                        std::thread::sleep(std::time::Duration::from_millis(2));
                                    }
                                }
                                ProcessResult::None => {
                                    // Don't emit anything (consumed by layer switch, etc.)
                                }
                            }
                        } else {
                            // Unsupported key, pass through unchanged
                            virtual_device.emit(&[ev])?;
                        }
                    } else {
                        // Non-key event (SYN, etc.), pass through
                        virtual_device.emit(&[ev])?;
                    }
                }

            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No events available - check for DT timeouts
                // This allows hold detection to work even when no keys are being pressed
                let timeout_result = keymap.check_dt_timeouts();
                match timeout_result {
                    ProcResult::MultipleEvents(events) => {
                        // Emit timeout events (hold first action, single-tap, etc.)
                        for (key, pressed) in events {
                            let key_evdev = Key::new(key.code());
                            let event = InputEvent::new_now(
                                EventType::KEY,
                                key_evdev.code(),
                                i32::from(pressed),
                            );
                            virtual_device.emit(&[event])?;
                        }
                    }
                    _ => {
                        // No timeouts to process
                    }
                }

                // A stroke held past its timeout, or a held prefix gone idle, is typed now
                match keymap.check_steno_timeouts() {
                    ProcResult::TypeString(text, add_enter) => {
                        type_string(&mut virtual_device, &mut pacer, &text, add_enter)?;
                    }
                    ProcResult::Retype { backspaces, text } => {
                        retype(&mut virtual_device, &mut pacer, backspaces, &text)?;
                    }
                    ProcResult::Ops(ops) => {
                        emit_ops(&mut virtual_device, &mut pacer, &ops)?;
                    }
                    _ => {}
                }

                // Let the keyboard's other device threads in before sleeping
                drop(keymap);
                // Sleep briefly to avoid CPU spinning
                // 1ms sleep provides excellent responsiveness while preventing busy-wait
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            Err(e) => return Err(e.into()),
        }

        if escaped {
            // Give the keyboard back to the system
            let _ = device.ungrab();
            crate::steno::setup::notify_user(
                user_id,
                "keymux released a keyboard",
                &format!("Alt+F12 released {keyboard_name} from keymux. Replug it to use keymux again."),
            );
            return Ok(());
        }
    }
}

/// Watches one keyboard's raw key events for Alt+F12, the chord that releases it from keymux.
/// It only counts as the very first key press after the keyboard is attached: the first press
/// decides, and anything other than Alt+F12 disarms the chord for good. Either Alt counts.
#[derive(Debug)]
struct AltF12 {
    left: bool,
    right: bool,
    armed: bool,
}

impl Default for AltF12 {
    fn default() -> Self {
        Self {
            left: false,
            right: false,
            armed: true,
        }
    }
}

impl AltF12 {
    /// Feed one raw key event, where `value` is 1 for press, 0 for release, 2 for repeat.
    /// True when Alt+F12 is the first key press: the keyboard should be released.
    fn feed(&mut self, key: Key, value: i32) -> bool {
        if key == Key::KEY_LEFTALT {
            self.left = value != 0;
            return false;
        }
        if key == Key::KEY_RIGHTALT {
            self.right = value != 0;
            return false;
        }
        // Releases and repeats aren't presses, so they don't decide anything
        if value != 1 {
            return false;
        }
        let escape = self.armed && key == Key::KEY_F12 && (self.left || self.right);
        self.armed = false;
        escape
    }
}

/// Create a virtual uinput device that mimics the physical keyboard
fn create_virtual_device(physical_device: &Device, keyboard_name: &str) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<Key>::new();

    // Copy all supported keys from physical device
    if let Some(physical_keys) = physical_device.supported_keys() {
        for key in physical_keys {
            keys.insert(key);
        }
    }

    let try_build = |name: &str| -> Result<VirtualDevice> {
        Ok(VirtualDeviceBuilder::new()?
            .name(name)
            .with_keys(&keys)?
            .build()?)
    };

    // Try full name first, fall back to truncated if it fails (kernel name limit is 80 bytes)
    let full_name = format!("keymux: {keyboard_name}");
    try_build(&full_name).or_else(|_| {
        let max_name_len = 80 - "keymux: ".len();
        let truncated = if keyboard_name.len() > max_name_len {
            format!("keymux: {}...", &keyboard_name[..max_name_len])
        } else {
            full_name
        };
        try_build(&truncated)
    })
}

/// Release all keys on startup (before keymap exists) to fix hotplug stuck keys
fn release_all_keys_on_startup(virtual_device: &mut VirtualDevice) {
    use evdev::InputEvent;

    // Release all modifiers (most critical for stuck keys)
    let modifiers = [
        Key::KEY_LEFTCTRL,
        Key::KEY_RIGHTCTRL,
        Key::KEY_LEFTSHIFT,
        Key::KEY_RIGHTSHIFT,
        Key::KEY_LEFTALT,
        Key::KEY_RIGHTALT,
        Key::KEY_LEFTMETA,
        Key::KEY_RIGHTMETA,
    ];

    for key in &modifiers {
        let event = InputEvent::new_now(EventType::KEY, key.code(), 0);
        let _ = virtual_device.emit(&[event]);
    }

    // Release all letter keys (common for WASD/typing)
    let letters = [
        Key::KEY_A,
        Key::KEY_B,
        Key::KEY_C,
        Key::KEY_D,
        Key::KEY_E,
        Key::KEY_F,
        Key::KEY_G,
        Key::KEY_H,
        Key::KEY_I,
        Key::KEY_J,
        Key::KEY_K,
        Key::KEY_L,
        Key::KEY_M,
        Key::KEY_N,
        Key::KEY_O,
        Key::KEY_P,
        Key::KEY_Q,
        Key::KEY_R,
        Key::KEY_S,
        Key::KEY_T,
        Key::KEY_U,
        Key::KEY_V,
        Key::KEY_W,
        Key::KEY_X,
        Key::KEY_Y,
        Key::KEY_Z,
    ];

    for key in &letters {
        let event = InputEvent::new_now(EventType::KEY, key.code(), 0);
        let _ = virtual_device.emit(&[event]);
    }

    // Release common navigation/control keys
    let nav_keys = [
        Key::KEY_UP,
        Key::KEY_DOWN,
        Key::KEY_LEFT,
        Key::KEY_RIGHT,
        Key::KEY_SPACE,
        Key::KEY_ENTER,
        Key::KEY_TAB,
        Key::KEY_ESC,
    ];

    for key in &nav_keys {
        let event = InputEvent::new_now(EventType::KEY, key.code(), 0);
        let _ = virtual_device.emit(&[event]);
    }

    // Send final SYN_REPORT
    let syn_event = InputEvent::new_now(EventType::SYNCHRONIZATION, SYN_CODE, SYN_REPORT);
    let _ = virtual_device.emit(&[syn_event]);
}

/// Release all potentially held keys before shutdown
fn release_all_keys(virtual_device: &mut VirtualDevice, keymap: &KeymapProcessor) {
    use evdev::InputEvent;

    // Get all keys that the keymap thinks are held
    let held_keys = keymap.get_held_keys();

    if !held_keys.is_empty() {
        info!(
            "Gracefully releasing {} held key(s) before shutdown",
            held_keys.len()
        );

        // Release all held keys
        for keycode in held_keys {
            let evdev_key = Key::new(keycode.code());
            let event = InputEvent::new_now(EventType::KEY, evdev_key.code(), 0);
            let _ = virtual_device.emit(&[event]);
        }

        // Send SYN_REPORT
        let syn_event = InputEvent::new_now(EventType::SYNCHRONIZATION, SYN_CODE, SYN_REPORT);
        let _ = virtual_device.emit(&[syn_event]);
    }

    // Also release common modifiers as a safety measure
    let modifiers = [
        Key::KEY_LEFTCTRL,
        Key::KEY_RIGHTCTRL,
        Key::KEY_LEFTSHIFT,
        Key::KEY_RIGHTSHIFT,
        Key::KEY_LEFTALT,
        Key::KEY_RIGHTALT,
        Key::KEY_LEFTMETA,
        Key::KEY_RIGHTMETA,
    ];

    for key in &modifiers {
        let event = InputEvent::new_now(EventType::KEY, key.code(), 0);
        let _ = virtual_device.emit(&[event]);
    }

    // Send final SYN_REPORT
    let syn_event = InputEvent::new_now(EventType::SYNCHRONIZATION, SYN_CODE, SYN_REPORT);
    let _ = virtual_device.emit(&[syn_event]);
}

/// The most input events allowed unread at once. The kernel keeps 63 per reader of a
/// keyboard (measured with a test device), and when one more arrives it throws away
/// everything queued for that reader, so letters vanish from the middle of a word. A keystroke
/// is 4 events, so this is 12 of them, with room left for keys sent elsewhere (modifiers).
const SAFE_EVENTS: usize = 48;

/// How long an event counts as possibly unread. An app or compositor busy for longer than
/// this can still lose events, but nothing sent at once ever can: up to `SAFE_EVENTS` go out
/// together, whatever the delay.
const UNREAD_WINDOW: std::time::Duration = std::time::Duration::from_millis(15);

/// Keeps what is sent to one virtual keyboard from outrunning whoever reads it. Output that fits
/// goes out at once. Only what doesn't fit waits, just until earlier events are old enough
/// to have been read.
#[derive(Default)]
struct Pacer {
    /// When events were sent, and how many, for the last `UNREAD_WINDOW`
    sent: std::collections::VecDeque<(std::time::Instant, usize)>,
}

impl Pacer {
    /// Plan when to send keystrokes of `sizes` events each, starting at `now`. Each part is
    /// the time to send and the keystrokes to send together. A keystroke is never split
    /// across two parts, so shift is never left held between them.
    fn schedule(
        &mut self,
        now: std::time::Instant,
        sizes: &[usize],
    ) -> Vec<(std::time::Instant, std::ops::Range<usize>)> {
        let mut parts = Vec::new();
        // Never before something already planned, so output stays in order
        let mut at = self.sent.back().map_or(now, |&(time, _)| time.max(now));
        let mut next = 0;
        while next < sizes.len() {
            while self
                .sent
                .front()
                .is_some_and(|&(time, _)| time + UNREAD_WINDOW <= at)
            {
                self.sent.pop_front();
            }
            let in_flight: usize = self.sent.iter().map(|&(_, events)| events).sum();
            let (mut end, mut used) = (next, 0);
            while end < sizes.len() && in_flight + used + sizes[end] <= SAFE_EVENTS {
                used += sizes[end];
                end += 1;
            }
            if end == next {
                if let Some(&(time, _)) = self.sent.front() {
                    // Nothing fits yet: wait for the oldest events to age out
                    at = time + UNREAD_WINDOW;
                    continue;
                }
                // A keystroke bigger than the whole budget still has to go
                (end, used) = (next + 1, sizes[next]);
            }
            parts.push((at, next..end));
            self.sent.push_back((at, used));
            next = end;
        }
        parts
    }
}

/// One key press and release, each followed by a SYN_REPORT, with shift held around it if needed
fn keystroke(key: Key, shift: bool) -> Vec<InputEvent> {
    let syn = || InputEvent::new_now(EventType::SYNCHRONIZATION, SYN_CODE, SYN_REPORT);
    let mut events = Vec::with_capacity(8);
    if shift {
        events.push(InputEvent::new_now(
            EventType::KEY,
            Key::KEY_LEFTSHIFT.code(),
            1,
        ));
        events.push(syn());
    }
    events.push(InputEvent::new_now(EventType::KEY, key.code(), 1));
    events.push(syn());
    events.push(InputEvent::new_now(EventType::KEY, key.code(), 0));
    events.push(syn());
    if shift {
        events.push(InputEvent::new_now(
            EventType::KEY,
            Key::KEY_LEFTSHIFT.code(),
            0,
        ));
        events.push(syn());
    }
    events
}

/// Emit keystrokes in order, as the pacer allows
fn emit_keystrokes(
    virtual_device: &mut VirtualDevice,
    pacer: &mut Pacer,
    keystrokes: &[Vec<InputEvent>],
) -> Result<()> {
    let sizes: Vec<usize> = keystrokes.iter().map(Vec::len).collect();
    for (at, range) in pacer.schedule(std::time::Instant::now(), &sizes) {
        if let Some(wait) = at.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(wait);
        }
        virtual_device.emit(&keystrokes[range].concat())?;
    }
    Ok(())
}

/// The keystrokes that delete `backspaces` characters then type `text`
fn retype_keystrokes(backspaces: usize, text: &str) -> Vec<Vec<InputEvent>> {
    let deletes = (0..backspaces).map(|_| keystroke(Key::KEY_BACKSPACE, false));
    let typed = text.chars().filter_map(|ch| {
        let (key, shift) = char_to_key(ch);
        key.map(|key| keystroke(key, shift))
    });
    deletes.chain(typed).collect()
}

/// Delete `backspaces` characters before the cursor, then type `text`
fn retype(
    virtual_device: &mut VirtualDevice,
    pacer: &mut Pacer,
    backspaces: usize,
    text: &str,
) -> Result<()> {
    emit_keystrokes(virtual_device, pacer, &retype_keystrokes(backspaces, text))
}

/// Do what a steno stroke asked for, in order: delete, type, press keys.
fn emit_ops(virtual_device: &mut VirtualDevice, pacer: &mut Pacer, ops: &[OutputOp]) -> Result<()> {
    let mut i = 0;
    while i < ops.len() {
        match &ops[i] {
            OutputOp::Backspaces(count) => {
                // The text that replaces what was deleted goes out with it
                if let Some(OutputOp::Text(text)) = ops.get(i + 1) {
                    retype(virtual_device, pacer, *count, text)?;
                    i += 1;
                } else {
                    retype(virtual_device, pacer, *count, "")?;
                }
            }
            OutputOp::Text(text) => type_string(virtual_device, pacer, text, false)?,
            OutputOp::Keys(events) => {
                let groups: Vec<Vec<InputEvent>> = events
                    .iter()
                    .map(|(key, pressed)| {
                        vec![
                            InputEvent::new_now(EventType::KEY, Key::new(key.code()).code(), i32::from(*pressed)),
                            InputEvent::new_now(EventType::SYNCHRONIZATION, SYN_CODE, SYN_REPORT),
                        ]
                    })
                    .collect();
                emit_keystrokes(virtual_device, pacer, &groups)?;
            }
            // Commands were run by the steno engine
            OutputOp::Command(_) => {}
        }
        i += 1;
    }
    Ok(())
}

/// Type a string by emitting key events for each character
fn type_string(
    virtual_device: &mut VirtualDevice,
    pacer: &mut Pacer,
    text: &str,
    _add_enter: bool,
) -> Result<()> {
    retype(virtual_device, pacer, 0, text)
}

/// Convert a character to an evdev Key and whether shift is needed
const fn char_to_key(ch: char) -> (Option<Key>, bool) {
    match ch {
        'a' => (Some(Key::KEY_A), false),
        'b' => (Some(Key::KEY_B), false),
        'c' => (Some(Key::KEY_C), false),
        'd' => (Some(Key::KEY_D), false),
        'e' => (Some(Key::KEY_E), false),
        'f' => (Some(Key::KEY_F), false),
        'g' => (Some(Key::KEY_G), false),
        'h' => (Some(Key::KEY_H), false),
        'i' => (Some(Key::KEY_I), false),
        'j' => (Some(Key::KEY_J), false),
        'k' => (Some(Key::KEY_K), false),
        'l' => (Some(Key::KEY_L), false),
        'm' => (Some(Key::KEY_M), false),
        'n' => (Some(Key::KEY_N), false),
        'o' => (Some(Key::KEY_O), false),
        'p' => (Some(Key::KEY_P), false),
        'q' => (Some(Key::KEY_Q), false),
        'r' => (Some(Key::KEY_R), false),
        's' => (Some(Key::KEY_S), false),
        't' => (Some(Key::KEY_T), false),
        'u' => (Some(Key::KEY_U), false),
        'v' => (Some(Key::KEY_V), false),
        'w' => (Some(Key::KEY_W), false),
        'x' => (Some(Key::KEY_X), false),
        'y' => (Some(Key::KEY_Y), false),
        'z' => (Some(Key::KEY_Z), false),

        'A' => (Some(Key::KEY_A), true),
        'B' => (Some(Key::KEY_B), true),
        'C' => (Some(Key::KEY_C), true),
        'D' => (Some(Key::KEY_D), true),
        'E' => (Some(Key::KEY_E), true),
        'F' => (Some(Key::KEY_F), true),
        'G' => (Some(Key::KEY_G), true),
        'H' => (Some(Key::KEY_H), true),
        'I' => (Some(Key::KEY_I), true),
        'J' => (Some(Key::KEY_J), true),
        'K' => (Some(Key::KEY_K), true),
        'L' => (Some(Key::KEY_L), true),
        'M' => (Some(Key::KEY_M), true),
        'N' => (Some(Key::KEY_N), true),
        'O' => (Some(Key::KEY_O), true),
        'P' => (Some(Key::KEY_P), true),
        'Q' => (Some(Key::KEY_Q), true),
        'R' => (Some(Key::KEY_R), true),
        'S' => (Some(Key::KEY_S), true),
        'T' => (Some(Key::KEY_T), true),
        'U' => (Some(Key::KEY_U), true),
        'V' => (Some(Key::KEY_V), true),
        'W' => (Some(Key::KEY_W), true),
        'X' => (Some(Key::KEY_X), true),
        'Y' => (Some(Key::KEY_Y), true),
        'Z' => (Some(Key::KEY_Z), true),

        '0' => (Some(Key::KEY_0), false),
        '1' => (Some(Key::KEY_1), false),
        '2' => (Some(Key::KEY_2), false),
        '3' => (Some(Key::KEY_3), false),
        '4' => (Some(Key::KEY_4), false),
        '5' => (Some(Key::KEY_5), false),
        '6' => (Some(Key::KEY_6), false),
        '7' => (Some(Key::KEY_7), false),
        '8' => (Some(Key::KEY_8), false),
        '9' => (Some(Key::KEY_9), false),

        '!' => (Some(Key::KEY_1), true),
        '@' => (Some(Key::KEY_2), true),
        '#' => (Some(Key::KEY_3), true),
        '$' => (Some(Key::KEY_4), true),
        '%' => (Some(Key::KEY_5), true),
        '^' => (Some(Key::KEY_6), true),
        '&' => (Some(Key::KEY_7), true),
        '*' => (Some(Key::KEY_8), true),
        '(' => (Some(Key::KEY_9), true),
        ')' => (Some(Key::KEY_0), true),

        ' ' => (Some(Key::KEY_SPACE), false),
        // Enter and Tab are keys, not characters, so they're typed by pressing those keys
        '\n' => (Some(Key::KEY_ENTER), false),
        '\t' => (Some(Key::KEY_TAB), false),
        '-' => (Some(Key::KEY_MINUS), false),
        '_' => (Some(Key::KEY_MINUS), true),
        '=' => (Some(Key::KEY_EQUAL), false),
        '+' => (Some(Key::KEY_EQUAL), true),
        '[' => (Some(Key::KEY_LEFTBRACE), false),
        '{' => (Some(Key::KEY_LEFTBRACE), true),
        ']' => (Some(Key::KEY_RIGHTBRACE), false),
        '}' => (Some(Key::KEY_RIGHTBRACE), true),
        '\\' => (Some(Key::KEY_BACKSLASH), false),
        '|' => (Some(Key::KEY_BACKSLASH), true),
        ';' => (Some(Key::KEY_SEMICOLON), false),
        ':' => (Some(Key::KEY_SEMICOLON), true),
        '\'' => (Some(Key::KEY_APOSTROPHE), false),
        '"' => (Some(Key::KEY_APOSTROPHE), true),
        ',' => (Some(Key::KEY_COMMA), false),
        '<' => (Some(Key::KEY_COMMA), true),
        '.' => (Some(Key::KEY_DOT), false),
        '>' => (Some(Key::KEY_DOT), true),
        '/' => (Some(Key::KEY_SLASH), false),
        '?' => (Some(Key::KEY_SLASH), true),
        '`' => (Some(Key::KEY_GRAVE), false),
        '~' => (Some(Key::KEY_GRAVE), true),

        _ => (None, false),
    }
}

#[cfg(test)]
mod alt_f12_tests {
    use super::*;

    /// Whether the chord fires for these events, in order, starting from a freshly attached keyboard.
    fn fires(events: &[(Key, i32)]) -> bool {
        let mut chord = AltF12::default();
        events.iter().any(|&(key, value)| chord.feed(key, value))
    }

    #[test]
    fn left_alt_then_f12_fires() {
        assert!(fires(&[(Key::KEY_LEFTALT, 1), (Key::KEY_F12, 1)]));
    }

    #[test]
    fn right_alt_then_f12_fires() {
        assert!(fires(&[(Key::KEY_RIGHTALT, 1), (Key::KEY_F12, 1)]));
    }

    #[test]
    fn f12_alone_does_not_fire() {
        assert!(!fires(&[(Key::KEY_F12, 1)]));
    }

    #[test]
    fn alt_released_before_f12_does_not_fire() {
        assert!(!fires(&[(Key::KEY_LEFTALT, 1), (Key::KEY_LEFTALT, 0), (Key::KEY_F12, 1)]));
    }

    #[test]
    fn holding_one_alt_while_pressing_the_other_keeps_it_held() {
        // Left alt down, right alt down, left alt up: right alt still holds, so F12 fires
        assert!(fires(&[
            (Key::KEY_LEFTALT, 1),
            (Key::KEY_RIGHTALT, 1),
            (Key::KEY_LEFTALT, 0),
            (Key::KEY_F12, 1),
        ]));
    }

    #[test]
    fn f12_repeat_and_release_do_not_fire() {
        assert!(!fires(&[(Key::KEY_LEFTALT, 1), (Key::KEY_F12, 2)]));
        assert!(!fires(&[(Key::KEY_LEFTALT, 1), (Key::KEY_F12, 0)]));
    }

    #[test]
    fn alt_f12_after_another_key_press_does_not_fire() {
        assert!(!fires(&[(Key::KEY_A, 1), (Key::KEY_A, 0), (Key::KEY_LEFTALT, 1), (Key::KEY_F12, 1)]));
    }

    #[test]
    fn alt_f12_after_a_modifier_press_does_not_fire() {
        // Ctrl+Alt+F12 is not the escape: Ctrl was the first press
        assert!(!fires(&[(Key::KEY_LEFTCTRL, 1), (Key::KEY_LEFTALT, 1), (Key::KEY_F12, 1)]));
    }

    #[test]
    fn alt_f12_after_a_long_gap_still_needs_to_be_first() {
        // Any press in between disarms it, even after the keyboard has been quiet
        assert!(!fires(&[(Key::KEY_LEFTSHIFT, 1), (Key::KEY_LEFTSHIFT, 0), (Key::KEY_LEFTALT, 1), (Key::KEY_F12, 1)]));
    }

    #[test]
    fn a_release_before_the_chord_does_not_disarm_it() {
        assert!(fires(&[(Key::KEY_A, 0), (Key::KEY_LEFTALT, 1), (Key::KEY_F12, 1)]));
    }
}
