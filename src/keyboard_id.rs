use crate::keycode::KeyCode;
use evdev::Device;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Check if a device is a keyboard by verifying it has letter keys
pub fn is_keyboard_device(device: &Device) -> bool {
    device.supported_keys().is_some_and(|keys| {
        keys.contains(evdev::Key::new(KeyCode::KC_A.code()))
            && keys.contains(evdev::Key::new(KeyCode::KC_Z.code()))
            && keys.contains(evdev::Key::new(KeyCode::KC_SPC.code()))
    })
}

/// Hardware-based keyboard identifier that persists across reboots
/// Format: vendor:product:version:bustype (e.g., "2e3c:c365:0110:0003")
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct KeyboardId(String);

/// Represents a logical keyboard with potentially multiple event devices
pub struct LogicalKeyboard {
    /// The base hardware ID
    pub id: KeyboardId,
    /// Human-readable name
    pub name: String,
    /// All event devices for this keyboard (sorted by input number)
    pub devices: Vec<(PathBuf, Device)>,
}

impl std::fmt::Debug for LogicalKeyboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogicalKeyboard")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("device_count", &self.devices.len())
            .finish()
    }
}

impl KeyboardId {
    /// Create a new KeyboardId from a string
    pub const fn new(id: String) -> Self {
        Self(id)
    }

    /// Check whether a config entry string matches this KeyboardId.
    ///
    /// Config entries can be in two forms:
    ///   - `"vendor:product:version:bustype"`         — matches any port (backwards compatible)
    ///   - `"vendor:product:version:bustype@port"`    — matches only that specific port
    ///
    /// This means old configs with bare IDs continue to work unchanged; the `@port`
    /// suffix is an optional extension for users who need to distinguish two identical
    /// keyboards on different USB ports.
    pub fn matches_config_entry(&self, entry: &str) -> bool {
        if entry.contains('@') {
            // Explicit port — exact match required
            self.0 == entry
        } else {
            // No port specified — match on the base id (strip our own port if present)
            let our_base = self.0.split('@').next().unwrap_or(&self.0);
            our_base == entry
        }
    }

    /// Create a hardware ID from an evdev device and its event path.
    ///
    /// Format: vendor:product:version:bustype[@usb-port]
    ///
    /// The USB port component (e.g. "@3-4.2") is appended when we can resolve it
    /// from sysfs. This disambiguates two identical keyboards plugged into different
    /// ports — they share the same vendor/product/version but sit on different ports.
    ///
    /// For non-USB devices (built-in, Bluetooth) the port component is omitted and
    /// the ID falls back to vendor:product:version:bustype as before.
    pub(crate) fn from_device(device: &Device, path: &Path) -> Self {
        let id_vendor = device.input_id().vendor();
        let id_product = device.input_id().product();
        let id_version = device.input_id().version();
        let id_bustype = device.input_id().bus_type();

        let base = format!(
            "{:04x}:{:04x}:{:04x}:{:04x}",
            id_vendor, id_product, id_version, id_bustype.0
        );

        // Append USB port topology when available so identical models on different
        // ports get distinct IDs.
        let hardware_id = match get_usb_port(path) {
            Some(port) => format!("{}@{}", base, port),
            None => base,
        };

        Self(hardware_id)
    }
}

impl std::fmt::Display for KeyboardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Extract input number from device path (e.g., /dev/input/event12 for input12)
fn get_input_number(path: &Path) -> Option<u32> {
    let event_name = path.file_name()?.to_str()?;
    if !event_name.starts_with("event") {
        return None;
    }

    // Read symlink from /sys/class/input/eventX to find inputY
    let sysfs_path = format!("/sys/class/input/{}", event_name);
    if let Ok(target) = fs::read_link(&sysfs_path) {
        // Target looks like: ../../devices/.../inputX/eventY
        let target_str = target.to_string_lossy();
        for component in target_str.split('/') {
            if let Some(stripped) = component.strip_prefix("input") {
                if let Ok(num) = stripped.parse::<u32>() {
                    return Some(num);
                }
            }
        }
    }

    None
}

/// Extract the USB port topology string from the sysfs symlink for a device path.
///
/// The sysfs symlink for /dev/input/eventN looks like:
///   ../../devices/pci.../usbX/X-Y.Z/X-Y.Z:I.F/0003:VVVV:PPPP.XXXX/input/inputN/eventN
///
/// We extract the "X-Y.Z" component (e.g. "3-4.2") which uniquely identifies the
/// physical USB port the device is plugged into, stable across reboots as long as
/// the keyboard stays in the same port.
///
/// Returns None for non-USB devices (e.g. built-in keyboards, Bluetooth), in which
/// case the caller falls back to the hardware ID alone.
fn get_usb_port(path: &Path) -> Option<String> {
    let event_name = path.file_name()?.to_str()?;
    if !event_name.starts_with("event") {
        return None;
    }

    let sysfs_path = format!("/sys/class/input/{}", event_name);
    let target = fs::read_link(&sysfs_path).ok()?;
    let target_str = target.to_string_lossy();

    // Walk path components looking for a USB port pattern like "3-4.2" or "3-4"
    // that is immediately followed by its config interface variant "3-4.2:1.0"
    // We want the bare port (no colon suffix).
    // Walk all components and keep updating — the last match is the deepest (most specific)
    // port. For a keyboard in a hub the path contains both the root hub path ("3-4") and the
    // physical port path ("3-4.2"); we want "3-4.2".
    let mut deepest: Option<String> = None;

    for component in target_str.split('/') {
        // USB port paths: digits, hyphen, digits, optional dot-separated sub-ports.
        // e.g. "1-2", "3-4.2", "3-4.2.1"  — NOT "3-4.2:1.0" (colon = interface suffix)
        if component.contains(':') {
            continue;
        }
        if component
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-' || c == '.')
            && component.contains('-')
            && component.chars().next().is_some_and(|c| c.is_ascii_digit())
        {
            deepest = Some(component.to_string());
        }
    }

    deepest
}

/// Find all keyboard devices and return them grouped by hardware ID
/// Each logical keyboard may have multiple event devices (input0, input1, etc.)
pub fn find_all_keyboards() -> HashMap<KeyboardId, LogicalKeyboard> {
    let mut device_groups: HashMap<KeyboardId, Vec<(PathBuf, Device, String, u32)>> =
        HashMap::new();

    for (path, device) in evdev::enumerate() {
        // Check if it's a keyboard device
        if is_keyboard_device(&device) {
            let name = device.name().unwrap_or("unknown").to_string();

            // Skip virtual keyboards created by this daemon
            if name.contains("Keyboard Middleware Virtual Keyboard") || name.starts_with("keymux: ")
            {
                tracing::debug!("Skipping virtual keyboard: {}", name);
                continue;
            }

            // Skip mice - check for mouse buttons
            if let Some(keys) = device.supported_keys() {
                let has_mouse_buttons = keys.contains(evdev::Key::BTN_TOOL_MOUSE)
                    || keys.contains(evdev::Key::BTN_TOOL_FINGER)
                    || keys.contains(evdev::Key::BTN_TOOL_PEN);

                if has_mouse_buttons {
                    tracing::debug!("Skipping mouse device (has mouse buttons): {}", name);
                    continue;
                }
            }

            // Skip mice - check for relative axes (mouse movement)
            if let Some(rel_axes) = device.supported_relative_axes() {
                let has_mouse_axes = rel_axes.contains(evdev::RelativeAxisType::REL_X)
                    || rel_axes.contains(evdev::RelativeAxisType::REL_Y);

                if has_mouse_axes {
                    tracing::debug!("Skipping mouse device (has relative axes): {}", name);
                    continue;
                }
            }

            // Get hardware ID, incorporating USB port for same-model disambiguation
            let id = KeyboardId::from_device(&device, &path);

            // Get input number for sorting
            let input_num = get_input_number(&path).unwrap_or(999);

            tracing::debug!(
                "Found keyboard device: '{}' at {} (ID: {}, input: {})",
                name,
                path.display(),
                id,
                input_num
            );

            device_groups
                .entry(id)
                .or_default()
                .push((path, device, name, input_num));
        }
    }

    // Convert grouped devices into LogicalKeyboards
    let mut keyboards = HashMap::new();
    for (id, mut devices) in device_groups {
        // Sort by input number (lowest first)
        devices.sort_by_key(|(_, _, _, input_num)| *input_num);

        // Use name from first device (lowest input number)
        let name = devices[0].2.clone();
        let lowest_input = devices[0].3;

        tracing::info!(
            "Logical keyboard '{}' (ID: {}) has {} device(s), lowest input: input{}",
            name,
            id,
            devices.len(),
            lowest_input
        );

        let logical_kb = LogicalKeyboard {
            id: id.clone(),
            name,
            devices: devices
                .into_iter()
                .map(|(path, dev, _, _)| (path, dev))
                .collect(),
        };

        keyboards.insert(id, logical_kb);
    }

    keyboards
}
