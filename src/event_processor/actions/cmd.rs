#![allow(clippy::branches_sharing_code, clippy::option_if_let_else)]

use crate::config::KeyAction;
use crate::event_processor::actions::{EmitResult, HandleContext, HeldAction};
use crate::keycode::KeyCode;

fn needs_shell(cmd: &str) -> bool {
    cmd.contains(' ')
        || cmd.contains(';')
        || cmd.contains('|')
        || cmd.contains('&')
        || cmd.contains('<')
        || cmd.contains('>')
        || cmd.contains('$')
        || cmd.contains('(')
        || cmd.contains(')')
}

fn get_user_info(uid: u32) -> Option<(String, std::path::PathBuf)> {
    std::process::Command::new("getent")
        .args(["passwd", &uid.to_string()])
        .output()
        .ok()
        .and_then(|output| {
            if output.status.success() {
                let passwd = String::from_utf8_lossy(&output.stdout);
                let parts: Vec<&str> = passwd.split(':').collect();
                if parts.len() >= 6 {
                    let username = parts[0].to_string();
                    let home = std::path::PathBuf::from(parts[5]);
                    return Some((username, home));
                }
                None
            } else {
                None
            }
        })
}

fn spawn_command(
    cmd: &str,
    config_dir: &std::path::Path,
    username: Option<&str>,
) -> std::io::Result<std::process::Child> {
    if needs_shell(cmd) {
        match username {
            Some(user) => std::process::Command::new("runuser")
                .args(["-u", user, "--", "/bin/bash", "-c", cmd])
                .current_dir(config_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn(),
            None => std::process::Command::new("/bin/bash")
                .arg("-c")
                .arg(cmd)
                .current_dir(config_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn(),
        }
    } else {
        match username {
            Some(user) => std::process::Command::new("runuser")
                .args(["-u", user, "--", cmd])
                .current_dir(config_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn(),
            None => std::process::Command::new(cmd)
                .current_dir(config_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .spawn(),
        }
    }
}

pub fn emit_cmd(
    action: &KeyAction,
    _keycode: KeyCode,
    ctx: &mut HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    match action {
        KeyAction::CMD(command) => {
            let cmd = command.clone();
            let config_dir = ctx.config_dir.to_path_buf();
            let user_id = ctx.user_id;

            std::thread::spawn(move || {
                let user_home = get_user_info(user_id).map(|(_, h)| h);

                let final_cmd = if cmd.starts_with('~') {
                    if let Some(home) = &user_home {
                        cmd.replacen('~', &home.to_string_lossy(), 1)
                    } else {
                        cmd
                    }
                } else {
                    cmd
                };

                let user_info = get_user_info(user_id);
                let username = user_info.as_ref().map(|(u, _)| u.as_str());

                if let Err(e) = spawn_command(&final_cmd, &config_dir, username) {
                    tracing::error!("Failed to execute command '{}': {}", final_cmd, e);
                }
            });
            (EmitResult::None, None)
        }
        _ => (EmitResult::None, None),
    }
}

pub fn unemit_cmd(
    _action: &KeyAction,
    _held_action: HeldAction,
    _keycode: KeyCode,
    _ctx: &mut HandleContext<'_>,
) -> EmitResult {
    EmitResult::None
}
