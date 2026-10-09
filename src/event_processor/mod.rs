use crate::keyboard_id::KeyboardId;
use crate::keycode::KeyCode;
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

    // SAFETY: Release all keys immediately on startup to prevent stuck keys
    // This fixes the hotplug bug where keys remain held after reconnection
    release_all_keys_on_startup(&mut virtual_device);
    info!("Released all keys on startup for safety: {}", keyboard_name);

    // Track last save time for periodic stats saving
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
        match device.fetch_events() {
            Ok(events) => {
                for ev in events {
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
                                    retype(&mut virtual_device, backspaces, &text)?;
                                }
                                ProcessResult::TypeString(text, add_enter) => {
                                    // Type out the string character by character
                                    type_string(&mut virtual_device, &text, add_enter)?;
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
                        type_string(&mut virtual_device, &text, add_enter)?;
                    }
                    ProcResult::Retype { backspaces, text } => {
                        retype(&mut virtual_device, backspaces, &text)?;
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

/// Delay between each typed character's key events. Emitting a whole word in one
/// burst with zero spacing lets the consuming app's input queue drop or coalesce
/// events under load, truncating the output unpredictably.
const TYPE_CHAR_DELAY: std::time::Duration = std::time::Duration::from_millis(1);

/// Delete `backspaces` characters before the cursor, then type `text`
fn retype(virtual_device: &mut VirtualDevice, backspaces: usize, text: &str) -> Result<()> {
    for _ in 0..backspaces {
        let events = [
            InputEvent::new_now(EventType::KEY, Key::KEY_BACKSPACE.code(), 1),
            InputEvent::new_now(EventType::SYNCHRONIZATION, SYN_CODE, SYN_REPORT),
        ];
        virtual_device.emit(&events)?;
        std::thread::sleep(TYPE_CHAR_DELAY);
        let events = [
            InputEvent::new_now(EventType::KEY, Key::KEY_BACKSPACE.code(), 0),
            InputEvent::new_now(EventType::SYNCHRONIZATION, SYN_CODE, SYN_REPORT),
        ];
        virtual_device.emit(&events)?;
        std::thread::sleep(TYPE_CHAR_DELAY);
    }
    type_string(virtual_device, text, false)
}

/// Type a string by emitting key events for each character, pacing each one so the
/// consuming app's input queue can keep up.
fn type_string(virtual_device: &mut VirtualDevice, text: &str, _add_enter: bool) -> Result<()> {
    for ch in text.chars() {
        let (key, needs_shift) = char_to_key(ch);

        if let Some(key) = key {
            let mut events = Vec::with_capacity(8);

            // Press shift if needed
            if needs_shift {
                events.push(InputEvent::new_now(
                    EventType::KEY,
                    Key::KEY_LEFTSHIFT.code(),
                    1,
                ));
                events.push(InputEvent::new_now(
                    EventType::SYNCHRONIZATION,
                    SYN_CODE,
                    SYN_REPORT,
                ));
            }

            // Press key
            events.push(InputEvent::new_now(EventType::KEY, key.code(), 1));
            events.push(InputEvent::new_now(
                EventType::SYNCHRONIZATION,
                SYN_CODE,
                SYN_REPORT,
            ));

            // Release key
            events.push(InputEvent::new_now(EventType::KEY, key.code(), 0));
            events.push(InputEvent::new_now(
                EventType::SYNCHRONIZATION,
                SYN_CODE,
                SYN_REPORT,
            ));

            // Release shift if needed
            if needs_shift {
                events.push(InputEvent::new_now(
                    EventType::KEY,
                    Key::KEY_LEFTSHIFT.code(),
                    0,
                ));
                events.push(InputEvent::new_now(
                    EventType::SYNCHRONIZATION,
                    SYN_CODE,
                    SYN_REPORT,
                ));
            }

            virtual_device.emit(&events)?;
            std::thread::sleep(TYPE_CHAR_DELAY);
        }
    }

    Ok(())
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
