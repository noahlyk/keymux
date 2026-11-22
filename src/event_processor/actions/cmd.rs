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
pub(crate) fn command(cmd: &str, config_dir: &Path, user: Option<&str>) -> Command {
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
    launch(cmd, config_dir, user_info(uid));
}

/// The command to start for `cmd` as `user` (name and home): a leading `~` is the user's home.
fn prepared(cmd: &str, config_dir: &Path, user: Option<&(String, PathBuf)>) -> (String, Command) {
    let cmd = match (user, cmd.strip_prefix('~')) {
        (Some((_, home)), Some(rest)) => format!("{}{rest}", home.to_string_lossy()),
        _ => cmd.to_string(),
    };
    let built = command(&cmd, config_dir, user.map(|(name, _)| name.as_str()));
    (cmd, built)
}

fn launch(cmd: &str, config_dir: &Path, user: Option<(String, PathBuf)>) {
    let (cmd, mut command) = prepared(cmd, config_dir, user.as_ref());
    match command.spawn() {
        Ok(mut child) => {
            let _ = child.wait();
        }
        Err(e) => tracing::error!("Failed to execute command '{cmd}': {e}"),
    }
}

/// Like [`run`] for callers that must never act as root: with no uid, or a user that is root or
/// cannot be looked up, nothing runs and the reason comes back.
pub(crate) fn run_as_user(cmd: &str, config_dir: &Path, uid: Option<u32>) -> Result<(), String> {
    let Some(uid) = uid.filter(|uid| *uid != 0) else {
        return Err("no non-root user id to run it as".to_string());
    };
    user_info(uid)
        .filter(|(name, _)| name != "root")
        .map_or_else(
            || {
                Err(format!(
                    "user id {uid} has no non-root passwd entry to run it as"
                ))
            },
            |user| {
                launch(cmd, config_dir, Some(user));
                Ok(())
            },
        )
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
    fn every_caller_builds_the_same_runuser_command_and_none_runs_as_root_by_default() {
        // `CMD` and the steno commands both start what `prepared` builds
        let user = ("fib".to_string(), PathBuf::from("/home/fib"));
        let (cmd, built) = prepared("~/bin/pw.sh x", Path::new("/c"), Some(&user));
        assert_eq!(cmd, "/home/fib/bin/pw.sh x");
        assert_eq!(
            argv(&built),
            [
                "runuser",
                "-u",
                "fib",
                "--",
                "/bin/bash",
                "-c",
                "/home/fib/bin/pw.sh x"
            ]
        );
        assert_eq!(built.get_current_dir(), Some(Path::new("/c")));
    }

    #[test]
    fn running_as_the_user_is_refused_without_a_non_root_user() {
        let why = |uid| run_as_user("true", Path::new("/"), uid).unwrap_err();
        assert!(why(None).contains("no non-root user id"), "{}", why(None));
        assert!(
            why(Some(0)).contains("no non-root user id"),
            "{}",
            why(Some(0))
        );
        assert!(
            why(Some(u32::MAX - 1)).contains("no non-root passwd entry"),
            "{}",
            why(Some(u32::MAX - 1))
        );
    }

    #[test]
    fn the_running_user_is_found_by_uid() {
        // SAFETY: getuid has no preconditions and cannot fail
        let uid = unsafe { libc::getuid() };
        let (name, home) = user_info(uid).expect("the current user is in passwd");
        assert!(!name.is_empty(), "no name");
        assert!(home.is_absolute(), "{}", home.display());
    }
}
