use crate::keyboard_id::KeyboardId;
pub use actions::ProcessResult;
use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, Device, Key};
pub use keymap::KeymapProcessor;
use pump::{Pump, Step};
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use tracing::{error, info};

pub mod actions;
pub mod adaptive;
mod debounce;
#[cfg(test)]
mod emission_tests;
pub mod keymap;
pub mod layer_stack;
mod output;
mod pump;
#[cfg(test)]
mod steno_tests;

#[cfg(test)]
pub(crate) use output::{char_to_key, retype_keystrokes, Pacer, SAFE_EVENTS, UNREAD_WINDOW};

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

    // Reads must not block: with nothing queued the loop runs the timeouts and waits itself
    let fd = device.as_raw_fd();
    // SAFETY: fd is the open device's own descriptor, and only its flags are touched
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    device.grab().context("Failed to grab device")?;
    info!("Grabbed device: {}", keyboard_name);

    let mut virtual_device = create_virtual_device(device, keyboard_name)?;
    info!("Created virtual device for: {}", keyboard_name);

    // A keyboard replugged with keys down would otherwise leave them stuck
    output::release_all_keys_on_startup(&mut virtual_device);
    info!("Released all keys on startup for safety: {}", keyboard_name);

    let mut pump = Pump::new(
        keyboard_name.to_string(),
        user_id,
        shutdown_rx,
        game_mode_rx,
        save_stats_rx,
    );
    loop {
        match pump.step(shared_keymap, device, &mut virtual_device)? {
            Step::Read | Step::Idle => {}
            Step::Shutdown => {
                let _ = device.ungrab();
                info!("Device ungrabbed and released for: {keyboard_name}");
                return Ok(());
            }
            Step::Escaped => {
                // Give the keyboard back to the system
                let _ = device.ungrab();
                crate::steno::setup::notify_user(
                    user_id,
                    "keymux released a keyboard",
                    &format!(
                        "Alt+F12 released {keyboard_name} from keymux. Replug it to use keymux again."
                    ),
                );
                return Ok(());
            }
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
