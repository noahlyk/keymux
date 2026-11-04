//! CMD: run a command when the key goes down, as the user the daemon serves, off the typing
//! thread.

use crate::event_processor::actions::{EmitResult, HandleContext, HeldAction};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Anything a shell would interpret means the command runs through one.
fn needs_shell(cmd: &str) -> bool {
    cmd.contains([' ', ';', '|', '&', '<', '>', '$', '(', ')'])
}

/// The name and home directory of a uid, from the passwd database.
fn user_info(uid: u32) -> Option<(String, PathBuf)> {
    let output = Command::new("getent")
        .args(["passwd", &uid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let passwd = String::from_utf8_lossy(&output.stdout);
    let mut fields = passwd.split(':');
    let name = fields.next()?.to_string();
    let home = PathBuf::from(fields.nth(4)?);
    Some((name, home))
}

/// The command to run: as `user` when one is known, through a shell when it needs one.
fn command(cmd: &str, config_dir: &Path, user: Option<&str>) -> Command {
    let mut argv: Vec<&str> = Vec::new();
    if let Some(user) = user {
        argv.extend(["runuser", "-u", user, "--"]);
    }
    if needs_shell(cmd) {
        argv.extend(["/bin/bash", "-c"]);
    }
    argv.push(cmd);
    let mut command = Command::new(argv[0]);
    command
        .args(&argv[1..])
        .current_dir(config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    command
}

/// Look the user up, expand a leading `~` to their home, run, and wait for the process so
/// it is reaped rather than left a zombie for as long as the daemon runs.
fn run(cmd: &str, config_dir: &Path, uid: u32) {
    let user = user_info(uid);
    let cmd = match (&user, cmd.strip_prefix('~')) {
        (Some((_, home)), Some(rest)) => format!("{}{rest}", home.to_string_lossy()),
        _ => cmd.to_string(),
    };
    let name = user.as_ref().map(|(name, _)| name.as_str());
    match command(&cmd, config_dir, name).spawn() {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => tracing::error!("Failed to execute command '{cmd}': {e}"),
    }
}

pub fn emit_cmd(cmd: &str, ctx: &HandleContext<'_>) -> (EmitResult, Option<HeldAction>) {
    let cmd = cmd.to_string();
    let config_dir = ctx.config_dir.to_path_buf();
    let uid = ctx.user_id;
    std::thread::spawn(move || run(&cmd, &config_dir, uid));
    (EmitResult::None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(command: &Command) -> Vec<String> {
        std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_bare_program_runs_directly_and_a_command_line_runs_through_a_shell() {
        assert_eq!(argv(&command("firefox", Path::new("/"), None)), ["firefox"]);
        assert_eq!(
            argv(&command("notify-send hi", Path::new("/"), None)),
            ["/bin/bash", "-c", "notify-send hi"]
        );
    }

    #[test]
    fn a_known_user_runs_it_as_them() {
        assert_eq!(
            argv(&command("ls | wc", Path::new("/"), Some("fib"))),
            ["runuser", "-u", "fib", "--", "/bin/bash", "-c", "ls | wc"]
        );
        assert_eq!(
            argv(&command("firefox", Path::new("/"), Some("fib"))),
            ["runuser", "-u", "fib", "--", "firefox"]
        );
    }

    #[test]
    fn the_running_user_is_found_by_uid() {
        // SAFETY: getuid has no preconditions and cannot fail
        let uid = unsafe { libc::getuid() };
        let (name, home) = user_info(uid).expect("the current user is in passwd");
        assert!(!name.is_empty());
        assert!(home.is_absolute(), "{}", home.display());
    }
}
