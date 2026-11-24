use crate::keyboard_id::KeyboardId;
pub use actions::ProcessResult;
use anyhow::{Context, Result};
use control::Controlled;
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, Device, Key};
pub use keymap::KeymapProcessor;
use merge::{Arrival, Merge};
use pump::{Pump, Step};
use std::collections::HashSet;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing::{error, info};

pub mod actions;
pub mod adaptive;
pub mod control;
mod debounce;
#[cfg(test)]
mod emission_tests;
pub mod keymap;
pub mod layer_stack;
pub mod merge;
#[cfg(test)]
mod merge_tests;
mod output;
mod pump;
#[cfg(test)]
mod steno_tests;
#[cfg(test)]
mod unicode_tests;

#[cfg(test)]
pub(crate) use output::{char_to_key, retype_taps, tap_events, Pacer, SAFE_EVENTS, UNREAD_WINDOW};

/// Run the event processor loop for a single keyboard event file.
///
/// This is called directly inside the thread spawned by the daemon.
/// It blocks until the keyboard is unplugged (ENODEV), a shutdown signal
/// is received, or an unrecoverable error occurs.
pub fn run_processor(
    keyboard_id: KeyboardId,
    mut device: Device,
    keyboard_name: String,
    keymap: Arc<Mutex<KeymapProcessor>>,
    user_id: u32,
    control: Controlled,
) {
    if let Err(e) = run_event_processor(
        &keyboard_id,
        &mut device,
        &keyboard_name,
        &keymap,
        user_id,
        control,
    ) {
        error!("Event processor for {} failed: {}", keyboard_id, e);
    }
    info!("Event processor thread exiting for: {}", keyboard_id);
}

fn run_event_processor(
    keyboard_id: &KeyboardId,
    device: &mut Device,
    keyboard_name: &str,
    shared_keymap: &Arc<Mutex<KeymapProcessor>>,
    user_id: u32,
    control: Controlled,
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

    let mut pump = Pump::new(keyboard_name.to_string(), user_id, control);
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

/// Run the event processor loop of a keyboard group.
///
/// Every member device, one keymap, one
/// virtual keyboard, one thread. Members that appear or go away while it runs arrive on
/// `arrivals`; it returns when told to stop, when Alt+F12 releases the group, or when the last
/// member is gone.
pub fn run_group(
    group: &str,
    members: Vec<(PathBuf, Device)>,
    arrivals: crossbeam_channel::Receiver<Arrival<Device>>,
    roster: Arc<Mutex<HashSet<PathBuf>>>,
    keymap: &Arc<Mutex<KeymapProcessor>>,
    user_id: u32,
    control: Controlled,
) {
    if let Err(e) = run_group_loop(group, members, arrivals, roster, keymap, user_id, control) {
        error!("Event processor for group {} failed: {}", group, e);
    }
    info!("Event processor thread exiting for group: {}", group);
}

fn run_group_loop(
    group: &str,
    members: Vec<(PathBuf, Device)>,
    arrivals: crossbeam_channel::Receiver<Arrival<Device>>,
    roster: Arc<Mutex<HashSet<PathBuf>>>,
    shared_keymap: &Arc<Mutex<KeymapProcessor>>,
    user_id: u32,
    control: Controlled,
) -> Result<()> {
    let name = format!("group {group}");
    let mut merge = Merge::with_roster(arrivals, roster.clone());
    let mut keys = AttributeSet::<Key>::new();
    for (path, mut device) in members {
        keys_of(&device, &mut keys);
        match prepare_member(&mut device) {
            Ok(()) => {
                info!("Grabbed {} for {}", path.display(), name);
                if merge.join(path.clone(), device).is_err() {
                    error!(
                        "{} has too many devices; not using {}",
                        name,
                        path.display()
                    );
                }
            }
            Err(e) => {
                error!("{}: {}: {:#}", name, path.display(), e);
                crate::lock(&roster).remove(&path);
            }
        }
    }
    if merge.is_empty() {
        anyhow::bail!("no member of {name} could be grabbed");
    }

    let mut virtual_device = build_virtual_device(&keys, &name)?;
    info!("Created virtual device for: {}", name);
    output::release_all_keys_on_startup(&mut virtual_device);

    let mut pump = Pump::new(name.clone(), user_id, control);
    loop {
        match pump.step(shared_keymap, &mut merge, &mut virtual_device)? {
            Step::Read | Step::Idle => {}
            Step::Shutdown => {
                merge.release_all();
                info!("Every device of {name} ungrabbed and released");
                return Ok(());
            }
            Step::Escaped => {
                merge.release_all();
                crate::steno::setup::notify_user(
                    user_id,
                    "keymux released a keyboard group",
                    &format!(
                        "Alt+F12 released every keyboard of {name} from keymux. Replug one to use keymux again."
                    ),
                );
                return Ok(());
            }
        }
    }
}

/// Non-blocking reads, and the device grabbed: what a member needs before it is read.
fn prepare_member(device: &mut Device) -> Result<()> {
    let fd = device.as_raw_fd();
    // SAFETY: fd is the open device's own descriptor, and only its flags are touched
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    device.grab().context("Failed to grab device")
}

/// Open a device that joins a running group: made ready as at the start.
pub fn open_member_device(path: &std::path::Path) -> Result<Device> {
    let mut device =
        Device::open(path).with_context(|| format!("Failed to open device: {}", path.display()))?;
    prepare_member(&mut device)?;
    Ok(device)
}

fn keys_of(device: &Device, keys: &mut AttributeSet<Key>) {
    if let Some(physical_keys) = device.supported_keys() {
        for key in physical_keys {
            keys.insert(key);
        }
    }
}

/// Create a virtual uinput device that mimics the physical keyboard
fn create_virtual_device(physical_device: &Device, keyboard_name: &str) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<Key>::new();

    // Copy all supported keys from physical device
    keys_of(physical_device, &mut keys);
    build_virtual_device(&keys, keyboard_name)
}

/// The virtual keyboard with `keys` and the media keys.
fn build_virtual_device(keys: &AttributeSet<Key>, keyboard_name: &str) -> Result<VirtualDevice> {
    let mut keys = keys.clone();

    // Media keys too, for `{#AudioPlay}` and the like: a device only sends the keys it advertises
    for key in crate::keycode::KeyCode::ALL {
        if key.category() == crate::keycode::KeyCategory::Media {
            keys.insert(Key::new(key.code()));
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
