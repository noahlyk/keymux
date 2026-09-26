use crate::config::{Config, EnableDisable, EnabledKeyboardEntry, EnabledKeyboards};
use crate::keyboard_id::{find_all_keyboards, is_keyboard_device};
use colored::Colorize;
use std::fs;
use std::path::Path;

pub struct KeyboardDisplay {
    pub logical_keyboards: Vec<(
        crate::keyboard_id::KeyboardId,
        crate::keyboard_id::LogicalKeyboard,
    )>,
    pub terminal_width: usize,
}

impl Default for KeyboardDisplay {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyboardDisplay {
    pub fn new() -> Self {
        Self {
            logical_keyboards: find_all_keyboards().into_iter().collect(),
            terminal_width: crate::ui::window::get_terminal_width(),
        }
    }

    pub fn print_keyboard_mapping(&self) {
        println!("{}", "🗺️  Keyboard Mapping:".bright_yellow().bold());

        if self.logical_keyboards.is_empty() {
            println!("  {}", "No keyboards found".bright_white());
            return;
        }

        println!(
            "  Logical keyboards: {}",
            self.logical_keyboards.len().to_string().bright_blue()
        );
        println!();

        // Calculate if table format fits
        let use_table_format = self.calculate_table_width() <= self.terminal_width;

        if use_table_format && !self.logical_keyboards.is_empty() {
            self.print_table_format();
        } else {
            self.print_paragraph_format();
        }
    }

    fn calculate_table_width(&self) -> usize {
        // Calculate required width for table
        let mut max_name_width = 4; // "Name"
        let mut max_id_width = 3; // "ID"

        for (id, logical_kb) in &self.logical_keyboards {
            max_name_width = max_name_width.max(logical_kb.name.len());
            max_id_width = max_id_width.max(id.to_string().len());
        }

        // Add spacing (2 spaces) + margins (4) + headers
        max_name_width + max_id_width + 20 + 6 // "Name" + "Hardware ID" + devices column + spacing
    }

    fn print_table_format(&self) {
        let mut max_name_width = 4;
        let mut max_id_width = 11; // "Hardware ID"

        for (id, logical_kb) in &self.logical_keyboards {
            max_name_width = max_name_width.max(logical_kb.name.len());
            max_id_width = max_id_width.max(id.to_string().len());
        }

        // Header
        println!(
            "  {:<width_name$}  {:<width_id$}",
            "Name".bright_white().bold(),
            "Hardware ID".bright_white().bold(),
            width_name = max_name_width,
            width_id = max_id_width
        );

        // Separator
        let separator_width = max_name_width + max_id_width + 4;
        println!("  {}", "─".repeat(separator_width).dimmed());

        // Data rows
        for (id, logical_kb) in &self.logical_keyboards {
            println!(
                "  {:<width_name$}  {:<width_id$}",
                logical_kb.name.bright_white(),
                id.to_string().dimmed(),
                width_name = max_name_width,
                width_id = max_id_width,
            );

            println!(
                "    {} {} device(s)",
                "Devices:".dimmed(),
                logical_kb.devices.len().to_string().bright_blue()
            );

            for (i, (path, _)) in logical_kb.devices.iter().enumerate() {
                let device_type = if i == 0 { "primary" } else { "secondary" };
                println!("      - {} ({})", path.display(), device_type.dimmed());
            }
            println!();
        }
    }

    fn print_paragraph_format(&self) {
        for (id, logical_kb) in &self.logical_keyboards {
            println!("    {}", logical_kb.name.bright_white());
            println!(
                "    {} {}",
                "Hardware ID:".dimmed(),
                id.to_string().dimmed()
            );
            println!(
                "    {} {} device(s)",
                "Devices:".dimmed(),
                logical_kb.devices.len().to_string().bright_blue()
            );

            for (i, (path, _)) in logical_kb.devices.iter().enumerate() {
                let device_type = if i == 0 { "primary" } else { "secondary" };
                println!("      - {} ({})", path.display(), device_type.dimmed());
            }
            println!();
        }
    }
}

pub struct ConfigDisplay {
    pub config_path: std::path::PathBuf,
    pub terminal_width: usize,
}

impl ConfigDisplay {
    pub fn new(config_path: std::path::PathBuf) -> Self {
        Self {
            config_path,
            terminal_width: crate::ui::window::get_terminal_width(),
        }
    }

    pub fn print_config_info(&self) {
        println!("{}", "⚙️  Configuration:".bright_yellow().bold());
        println!(
            "  Config Path: {}",
            self.config_path.display().to_string().bright_white()
        );

        if self.config_path.exists() {
            println!("  Status: {}", "✓ Exists".bright_green());

            match Config::load(&self.config_path) {
                Ok(config) => {
                    self.print_config_details(&config);
                }
                Err(e) => {
                    println!("  Status: {}", format!("✗ Error: {}", e).bright_red());
                }
            }
        } else {
            println!("  Status: {}", "✗ Not found".bright_red());
            println!(
                "  Hint: Run `keymux init` to create a starter config at {:?}",
                self.config_path
            );
        }
        println!();
    }

    fn print_config_details(&self, config: &Config) {
        // Enabled keyboards (normalize to handle legacy Some* variants)
        match config.enabled_keyboards.normalize() {
            EnabledKeyboards::ExplicitNone | EnabledKeyboards::SomeNone => {
                println!("  Enabled Keyboards: {}", "None (all disabled)".dimmed());
            }
            EnabledKeyboards::List(enabled_keyboards)
            | EnabledKeyboards::SomeList(enabled_keyboards) => {
                println!(
                    "  Enabled Keyboards: {}",
                    enabled_keyboards.len().to_string().bright_blue()
                );
                for kb in enabled_keyboards {
                    let display = match kb {
                        EnabledKeyboardEntry::Bare(pattern) => pattern.clone(),
                        EnabledKeyboardEntry::Explicit(pattern, action) => {
                            let action_str = match action {
                                EnableDisable::Enable => "Enable",
                                EnableDisable::Disable => "Disable",
                            };
                            format!("{}: {}", pattern, action_str)
                        }
                    };
                    println!("    - {}", display.bright_white());
                }
            }
        }

        // Tapping term
        println!(
            "  Tapping Term: {}ms",
            config.tapping_term_ms.to_string().bright_blue()
        );

        // Double tap window
        if config.mt_config.double_tap_then_hold {
            println!(
                "  Double Tap Window: {}ms",
                config
                    .mt_config
                    .double_tap_window_ms
                    .to_string()
                    .bright_blue()
            );
        }

        // Layers and remaps
        println!(
            "  Layers: {}",
            config.layers.len().to_string().bright_blue()
        );
        println!(
            "  Base Remaps: {}",
            config.remaps.len().to_string().bright_blue()
        );
        println!(
            "  Game Mode Remaps: {}",
            config.game_mode.remaps.len().to_string().bright_blue()
        );
    }
}

pub struct DeviceDisplay {
    pub terminal_width: usize,
}

impl Default for DeviceDisplay {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceDisplay {
    pub fn new() -> Self {
        Self {
            terminal_width: crate::ui::window::get_terminal_width(),
        }
    }

    pub fn print_device_watching(&self) {
        println!("{}", "👀 Device Watching:".bright_yellow().bold());

        // Check /dev/input directory
        if Path::new("/dev/input").exists() {
            println!("  /dev/input: {}", "✓ Accessible".bright_green());

            if let Ok(entries) = fs::read_dir("/dev/input") {
                let mut event_files = Vec::new();
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name.starts_with("event") {
                            event_files.push(path);
                        }
                    }
                }

                println!(
                    "  Event Files Found: {}",
                    event_files.len().to_string().bright_blue()
                );

                // Show keyboard event files
                self.print_keyboard_devices(&event_files);
            }
        } else {
            println!("  /dev/input: {}", "✗ Not accessible".bright_red());
            println!("  Make sure you're in the 'input' group");
        }
        println!();
    }

    fn print_keyboard_devices(&self, event_files: &[std::path::PathBuf]) {
        for event_file in event_files {
            if let Ok(device) = evdev::Device::open(event_file) {
                if is_keyboard_device(&device) {
                    let name = device.name().unwrap_or("unknown");
                    // Skip virtual keyboards created by the daemon
                    if name.contains("Keyboard Middleware Virtual Keyboard") {
                        continue;
                    }

                    let display = event_file.display().to_string();
                    println!("    - {}", display.bright_white());
                    println!("      Name: {}", name.dimmed());

                    let input_id = device.input_id();
                    println!(
                        "      ID: {:04x}:{:04x}:{:04x}:{:04x}",
                        input_id.vendor(),
                        input_id.product(),
                        input_id.version(),
                        input_id.bus_type().0
                    );
                }
            }
        }
    }
}

pub struct PermissionsDisplay {
    pub terminal_width: usize,
}

impl Default for PermissionsDisplay {
    fn default() -> Self {
        Self::new()
    }
}

impl PermissionsDisplay {
    pub fn new() -> Self {
        Self {
            terminal_width: crate::ui::window::get_terminal_width(),
        }
    }

    pub fn print_permissions_info(&self) {
        println!("{}", "🔒 Permissions:".bright_yellow().bold());

        // Check input group
        if let Ok(output) = std::process::Command::new("groups").output() {
            let groups_str = String::from_utf8_lossy(&output.stdout);
            let in_input_group = groups_str.contains("input");

            if in_input_group {
                println!("  Input Group: {}", "✓ Member".bright_green());
            } else {
                println!("  Input Group: {}", "✗ Not member".bright_red());
                println!("  Run: {}", "sudo usermod -a -G input $USER".bright_white());
            }
        }

        // Check if running as root
        let is_root = unsafe { libc::getuid() } == 0;
        if is_root {
            println!("  Root Access: {}", "✓ Running as root".bright_green());
        } else {
            println!("  Root Access: {}", "○ Running as user".dimmed());
        }

        println!();
    }
}

pub struct SessionDisplay {
    pub terminal_width: usize,
}

impl Default for SessionDisplay {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionDisplay {
    pub fn new() -> Self {
        Self {
            terminal_width: crate::ui::window::get_terminal_width(),
        }
    }

    pub fn print_user_sessions(&self) {
        if let Ok(output) = std::process::Command::new("loginctl")
            .args(["list-sessions", "--no-legend"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let sessions: Vec<_> = stdout.lines().collect();

            if sessions.is_empty() {
                println!("  {}", "No sessions found".dimmed());
            } else {
                println!(
                    "  Active Sessions: {}",
                    sessions.len().to_string().bright_blue()
                );

                for line in sessions {
                    println!("    {}", line.bright_white());
                }
            }
        } else {
            println!(
                "  {} {}",
                "Could not query sessions".dimmed(),
                "(loginctl not available)".dimmed()
            );
        }

        println!();
    }
}
