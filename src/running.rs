//! Which build the running keymux processes are, for the second line of `keymux --version`.
//!
//! A long-running keymux process keeps the binary it was started from, so after an upgrade
//! the daemon and its helpers are still the old build until they are restarted.
//! `keymux --version` says so:
//!
//! ```text
//! keymux 1.19.0+5.g7549a38
//! (daemon, overlay out-of-date, running 1.18.1+5.gf686c17)
//! ```
//!
//! How a running process's version is found, first hit wins:
//!
//! 1. The daemon answers [`IpcRequest::Version`] on its socket. A daemon from before that
//!    request existed drops the connection, which reads as "no answer" within the timeout.
//! 2. The version file the process wrote when it started ([`announce`]): the daemon's in
//!    `/run`, a helper's (overlay, window watcher) in the user's runtime folder. A file only
//!    counts for the process that wrote it (same pid and start time), so one left behind by
//!    a process that is gone, or a pid used again by another, is ignored.
//! 3. No version to read. The process is the same build when it runs the very file this
//!    binary is (same device and inode, not replaced). Otherwise it is reported as
//!    `running an older build`: it was started from a binary that has since been replaced,
//!    or from a build that predates version files.
//!
//! Processes are found by reading `/proc`, so nothing here needs a process to cooperate, and
//! everything is best effort: whatever cannot be read is left out, never an error.

use crate::ipc::{self, IpcRequest, IpcResponse};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The version this binary reports, worked out by `build.rs`.
pub const VERSION: &str = env!("KEYMUX_VERSION");

/// The daemon, as it is named in `keymux --version` and on its command line.
pub const DAEMON: &str = "daemon";

/// The long-running keymux processes, by subcommand (`keymux <role>`).
pub const ROLES: [&str; 7] = [
    DAEMON,
    "overlay",
    "niri-daemon",
    "hyprland-daemon",
    "sway-daemon",
    "i3-daemon",
    "bspwm-daemon",
];

/// How long `keymux --version` waits for the daemon's answer. The daemon answers this
/// request from its socket thread without waiting for the key loop, so a healthy daemon
/// answers in well under a millisecond; a busy or older one costs at most this.
const IPC_TIMEOUT: Duration = Duration::from_millis(25);

/// What a running process is known to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Build {
    /// It reported this version.
    Version(String),
    /// Its version cannot be read, and it is not this binary.
    Older,
}

/// One running keymux process, as found in `/proc`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Process {
    pid: u32,
    role: &'static str,
    /// Clock ticks since boot at which it started (`/proc/<pid>/stat`, field 22). With the
    /// pid it names one process for good: a pid used again has another start time.
    started: u64,
}

/// The file a process of `role` writes its version to.
///
/// The daemon runs as root and writes next to its socket in `/run`, world-readable. A helper
/// runs as the user and writes in the user's runtime folder, which root (under `sudo`) can
/// read too.
pub fn version_file(role: &str) -> PathBuf {
    let dir = if role == DAEMON {
        PathBuf::from("/run")
    } else {
        ipc::user_runtime_dir()
    };
    dir.join(format!("keymux-{role}.version"))
}

/// Called by a long-running process at startup: record which build it is.
///
/// Best effort. A process whose file could not be written still shows up correctly as long
/// as its binary has not been replaced (rule 3 in the module docs).
pub fn announce(role: &str) -> std::io::Result<()> {
    let pid = std::process::id();
    let started = start_time(Path::new("/proc"), pid).unwrap_or(0);
    write_version_file(&version_file(role), VERSION, pid, started)
}

/// Write `version`, `pid` and `started`, one per line, readable by everyone. Written to a
/// temporary name and renamed, so a reader never sees half a file.
fn write_version_file(path: &Path, version: &str, pid: u32, started: u64) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let tmp = path.with_extension(format!("version.{pid}.tmp"));
    let written = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o644))?;
        writeln!(file, "{version}\n{pid}\n{started}")?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The version in the file at `path`, if `process` is the one that wrote it.
fn read_version_file(path: &Path, process: &Process) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let version = lines.next()?.trim();
    let pid: u32 = lines.next()?.trim().parse().ok()?;
    let started: u64 = lines.next()?.trim().parse().ok()?;
    (!version.is_empty() && pid == process.pid && started == process.started)
        .then(|| version.to_string())
}

/// Field 22 of `/proc/<pid>/stat`. The command name (field 2) may hold spaces and brackets,
/// so the fields are counted from the last `)`.
fn start_time(proc: &Path, pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(proc.join(pid.to_string()).join("stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(19)?.parse().ok()
}

/// The running keymux processes with a role: every daemon, and the helpers that belong to
/// `uid` (another user's helpers keep their version files where this user cannot read them).
/// `skip` is this process.
fn find_processes(proc: &Path, uid: u32, skip: u32) -> Vec<Process> {
    let Ok(entries) = std::fs::read_dir(proc) else {
        return Vec::new();
    };
    let mut found: Vec<Process> = entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            if pid == skip {
                return None;
            }
            let dir = entry.path();
            if std::fs::read_to_string(dir.join("comm")).ok()?.trim_end() != "keymux" {
                return None;
            }
            let cmdline = std::fs::read(dir.join("cmdline")).ok()?;
            let subcommand = cmdline.split(|&b| b == 0).nth(1)?;
            let role = *ROLES.iter().find(|r| r.as_bytes() == subcommand)?;
            if role != DAEMON && std::fs::metadata(&dir).ok()?.uid() != uid {
                return None;
            }
            Some(Process {
                pid,
                role,
                started: start_time(proc, pid)?,
            })
        })
        .collect();
    found.sort_by_key(|p| (ROLES.iter().position(|r| *r == p.role), p.pid));
    found
}

/// Is `process` running the very file at `own_exe` (this binary), and is that file still in
/// place? A binary replaced by an upgrade reads as `/usr/bin/keymux (deleted)`.
fn runs_this_binary(proc: &Path, process: &Process, own_exe: &Path) -> bool {
    let exe = proc.join(process.pid.to_string()).join("exe");
    let Ok(target) = std::fs::read_link(&exe) else {
        return false;
    };
    if target.to_string_lossy().ends_with(" (deleted)") {
        return false;
    }
    match (std::fs::metadata(&exe), std::fs::metadata(own_exe)) {
        (Ok(theirs), Ok(ours)) => theirs.dev() == ours.dev() && theirs.ino() == ours.ino(),
        _ => false,
    }
}

/// Ask the daemon on the socket at `path` for its version. `None` when nothing answers in
/// time, or when what answers does not know the request (an older daemon hangs up).
pub fn daemon_version(path: &Path, timeout: Duration) -> Option<String> {
    match ipc::request_with_timeout(path, &IpcRequest::Version, timeout) {
        Some(IpcResponse::Version(version)) => Some(version),
        _ => None,
    }
}

/// Where [`running_builds`] looks, so tests can point it at folders of their own.
struct Sources<'a> {
    proc: &'a Path,
    own_exe: &'a Path,
    uid: u32,
    own_pid: u32,
    /// The daemon's answer over IPC, if it gave one.
    daemon_ipc: Option<String>,
    version_file: &'a dyn Fn(&str) -> PathBuf,
}

/// Every running keymux process whose build is known to differ or could differ: its role and
/// what it runs. Processes proven to be this very binary are left out.
fn running_builds(sources: &Sources) -> Vec<(&'static str, Build)> {
    let mut builds = Vec::new();
    if let Some(version) = &sources.daemon_ipc {
        builds.push((DAEMON, Build::Version(version.clone())));
    }
    for process in find_processes(sources.proc, sources.uid, sources.own_pid) {
        if process.role == DAEMON && sources.daemon_ipc.is_some() {
            continue;
        }
        if let Some(version) = read_version_file(&(sources.version_file)(process.role), &process) {
            builds.push((process.role, Build::Version(version)));
        } else if !runs_this_binary(sources.proc, &process, sources.own_exe) {
            builds.push((process.role, Build::Older));
        }
    }
    builds
}

/// The lines `keymux --version` prints under its own version: one per distinct build that is
/// not `own`, naming the processes that run it. Empty when every process matches.
pub fn stale_lines(own: &str, builds: &[(&str, Build)]) -> Vec<String> {
    let mut groups: Vec<(&Build, Vec<&str>)> = Vec::new();
    for (role, build) in builds {
        if matches!(build, Build::Version(version) if version == own) {
            continue;
        }
        match groups.iter_mut().find(|(b, _)| *b == build) {
            Some((_, roles)) if roles.contains(role) => {}
            Some((_, roles)) => roles.push(role),
            None => groups.push((build, vec![role])),
        }
    }
    groups
        .into_iter()
        .map(|(build, roles)| {
            let running = match build {
                Build::Version(version) => version.as_str(),
                Build::Older => "an older build",
            };
            format!("({} out-of-date, running {running})", roles.join(", "))
        })
        .collect()
}

/// The out-of-date lines for this machine, right now. Never fails and never takes long: see
/// [`IPC_TIMEOUT`], plus one pass over `/proc`.
pub fn stale_report() -> Vec<String> {
    let own_exe = std::env::current_exe().unwrap_or_default();
    let (uid, _) = crate::get_actual_user_uid();
    let builds = running_builds(&Sources {
        proc: Path::new("/proc"),
        own_exe: &own_exe,
        uid,
        own_pid: std::process::id(),
        daemon_ipc: daemon_version(&ipc::get_socket_path(), IPC_TIMEOUT),
        version_file: &version_file,
    });
    stale_lines(VERSION, &builds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;

    const OWN: &str = "1.19.0+5.g7549a38";
    const OLD: &str = "1.18.1+5.gf686c17";

    fn version(v: &str) -> Build {
        Build::Version(v.to_string())
    }

    #[test]
    fn nothing_is_printed_when_everything_matches_or_nothing_runs() {
        assert_eq!(stale_lines(OWN, &[]), Vec::<String>::new());
        let all = [(DAEMON, version(OWN)), ("overlay", version(OWN))];
        assert_eq!(stale_lines(OWN, &all), Vec::<String>::new());
    }

    #[test]
    fn stale_processes_on_one_version_share_a_line() {
        let builds = [
            (DAEMON, version(OLD)),
            ("overlay", version(OLD)),
            ("niri-daemon", version(OWN)),
        ];
        assert_eq!(
            stale_lines(OWN, &builds),
            ["(daemon, overlay out-of-date, running 1.18.1+5.gf686c17)"]
        );
        assert_eq!(
            stale_lines(OWN, &[(DAEMON, version(OLD))]),
            ["(daemon out-of-date, running 1.18.1+5.gf686c17)"]
        );
    }

    #[test]
    fn different_versions_get_a_line_each() {
        let builds = [
            (DAEMON, version(OLD)),
            ("overlay", version("1.17.0")),
            ("niri-daemon", Build::Older),
            ("sway-daemon", version(OLD)),
        ];
        assert_eq!(
            stale_lines(OWN, &builds),
            [
                "(daemon, sway-daemon out-of-date, running 1.18.1+5.gf686c17)",
                "(overlay out-of-date, running 1.17.0)",
                "(niri-daemon out-of-date, running an older build)",
            ]
        );
    }

    #[test]
    fn versions_are_compared_as_exact_strings() {
        // Same commit, but one has uncommitted changes: not the same build
        let dirty = format!("{OWN}.dirty");
        assert_eq!(
            stale_lines(&dirty, &[(DAEMON, version(OWN))]),
            [format!("(daemon out-of-date, running {OWN})")]
        );
        // A newer running process is named the same way: it is not this build
        assert_eq!(stale_lines(OLD, &[(DAEMON, version(OWN))]).len(), 1);
    }

    #[test]
    fn two_processes_of_one_role_are_named_once() {
        let builds = [("overlay", Build::Older), ("overlay", Build::Older)];
        assert_eq!(
            stale_lines(OWN, &builds),
            ["(overlay out-of-date, running an older build)"]
        );
    }

    #[test]
    fn a_version_file_round_trips_for_the_process_that_wrote_it() {
        let dir = TestDir::new("keymux-test-version-file");
        let path = dir.join("keymux-overlay.version");
        write_version_file(&path, OLD, 4242, 987_654).unwrap();
        let process = Process {
            pid: 4242,
            role: "overlay",
            started: 987_654,
        };
        assert_eq!(read_version_file(&path, &process).as_deref(), Some(OLD));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{OLD}\n4242\n987654\n")
        );
        // Everyone may read it: the daemon's is written by root and read by the user
        let mode = std::fs::metadata(&path).unwrap().mode();
        assert_eq!(mode & 0o777, 0o644);
        // Nothing but the file itself is left in the folder
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_version_file_from_another_process_is_ignored() {
        let dir = TestDir::new("keymux-test-version-stale");
        let path = dir.join("keymux-overlay.version");
        write_version_file(&path, OLD, 4242, 987_654).unwrap();
        let process = |pid, started| Process {
            pid,
            role: "overlay",
            started,
        };
        assert_eq!(read_version_file(&path, &process(4243, 987_654)), None);
        // The pid was used again by a process started later
        assert_eq!(read_version_file(&path, &process(4242, 987_655)), None);
        assert_eq!(
            read_version_file(&dir.join("missing"), &process(4242, 987_654)),
            None
        );
        for junk in ["", "1.0.0\n", "1.0.0\nnot-a-pid\n3\n", "\n4242\n987654\n"] {
            std::fs::write(&path, junk).unwrap();
            assert_eq!(
                read_version_file(&path, &process(4242, 987_654)),
                None,
                "{junk:?}"
            );
        }
    }

    #[test]
    fn writing_where_it_cannot_be_written_is_an_error_not_a_panic() {
        let dir = TestDir::new("keymux-test-version-nowhere");
        assert!(write_version_file(&dir.join("no/such/folder/x.version"), OWN, 1, 1).is_err());
    }

    /// A fake `/proc/<pid>`: `keymux <args>` started at tick `started`, running `exe`.
    fn fake_process(proc: &Path, pid: u32, comm: &str, args: &[&str], started: u64, exe: &Path) {
        let dir = proc.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        let mut cmdline = b"/usr/bin/keymux\0".to_vec();
        for arg in args {
            cmdline.extend(arg.as_bytes());
            cmdline.push(0);
        }
        std::fs::write(dir.join("cmdline"), cmdline).unwrap();
        // A command name with a space and a bracket, as the kernel would print it
        let stat = format!(
            "{pid} (key mux) x) S 1 {pid} {pid} 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 {started} 1 2 3"
        );
        std::fs::write(dir.join("stat"), stat).unwrap();
        std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
    }

    fn my_uid() -> u32 {
        unsafe { libc::getuid() }
    }

    #[test]
    fn processes_are_found_by_name_and_subcommand() {
        let proc = TestDir::new("keymux-test-proc-find");
        let exe = proc.join("bin");
        std::fs::write(&exe, "").unwrap();
        fake_process(&proc, 30, "keymux", &["overlay"], 300, &exe);
        fake_process(&proc, 10, "keymux", &["daemon", "--user", "me"], 100, &exe);
        fake_process(&proc, 20, "keymux", &["niri-daemon"], 200, &exe);
        // Not long-running roles, not keymux, this process itself, and not a process at all
        fake_process(&proc, 40, "keymux", &["wait-daemon"], 400, &exe);
        fake_process(&proc, 50, "keymux", &["steno", "lookup"], 500, &exe);
        fake_process(&proc, 60, "keymux", &[], 600, &exe);
        fake_process(&proc, 70, "bash", &["overlay"], 700, &exe);
        fake_process(&proc, 80, "keymux", &["overlay"], 800, &exe);
        std::fs::create_dir_all(proc.join("sys")).unwrap();

        let found = find_processes(&proc, my_uid(), 80);
        let summary: Vec<_> = found.iter().map(|p| (p.pid, p.role, p.started)).collect();
        assert_eq!(
            summary,
            [
                (10, "daemon", 100),
                (30, "overlay", 300),
                (20, "niri-daemon", 200)
            ]
        );

        // Another user's helpers are left out, the daemon (root's, normally) never is
        let found = find_processes(&proc, my_uid() + 1, 80);
        assert_eq!(found.iter().map(|p| p.pid).collect::<Vec<_>>(), [10]);

        assert_eq!(find_processes(&proc.join("missing"), my_uid(), 0), []);
    }

    #[test]
    fn this_process_is_found_in_the_real_proc() {
        let me = std::process::id();
        assert!(start_time(Path::new("/proc"), me).is_some_and(|t| t > 0));
    }

    /// The whole decision, on a fake `/proc`: file, then "is it this binary", then older.
    #[test]
    fn each_process_resolves_to_its_version_this_binary_or_an_older_build() {
        let proc = TestDir::new("keymux-test-proc-builds");
        let files = TestDir::new("keymux-test-proc-files");
        let own_exe = proc.join("keymux-new");
        let other_exe = proc.join("keymux-old");
        std::fs::write(&own_exe, "new").unwrap();
        std::fs::write(&other_exe, "old").unwrap();
        // What the kernel shows for a binary that an upgrade replaced
        let replaced = Path::new("/usr/bin/keymux (deleted)");

        // The daemon wrote a file: its version is read from it
        fake_process(&proc, 10, "keymux", &["daemon"], 100, &other_exe);
        write_version_file(&files.join("keymux-daemon.version"), OLD, 10, 100).unwrap();
        // The overlay's file is from an overlay that is gone, and its binary was replaced
        fake_process(&proc, 20, "keymux", &["overlay"], 200, replaced);
        write_version_file(&files.join("keymux-overlay.version"), OWN, 19, 150).unwrap();
        // No file, but it runs this very binary: the same build
        fake_process(&proc, 30, "keymux", &["niri-daemon"], 300, &own_exe);
        // No file and another binary: a build from before version files
        fake_process(&proc, 40, "keymux", &["sway-daemon"], 400, &other_exe);

        let version_file = |role: &str| files.join(format!("keymux-{role}.version"));
        let mut sources = Sources {
            proc: &proc,
            own_exe: &own_exe,
            uid: my_uid(),
            own_pid: 1,
            daemon_ipc: None,
            version_file: &version_file,
        };
        assert_eq!(
            running_builds(&sources),
            [
                ("daemon", version(OLD)),
                ("overlay", Build::Older),
                ("sway-daemon", Build::Older),
            ]
        );
        assert_eq!(
            stale_lines(OWN, &running_builds(&sources)),
            [
                "(daemon out-of-date, running 1.18.1+5.gf686c17)",
                "(overlay, sway-daemon out-of-date, running an older build)",
            ]
        );

        // The daemon's own answer wins over its file, and it is not listed twice
        sources.daemon_ipc = Some(OWN.to_string());
        assert_eq!(running_builds(&sources)[0], ("daemon", version(OWN)));
        assert_eq!(running_builds(&sources).len(), 3);
    }

    #[test]
    fn a_process_whose_binary_cannot_be_read_is_not_this_binary() {
        let proc = TestDir::new("keymux-test-proc-exe");
        let own_exe = proc.join("keymux-new");
        std::fs::write(&own_exe, "new").unwrap();
        // Root's daemon: /proc/<pid>/exe cannot be read by the user, here it is just missing
        let dir = proc.join("10");
        std::fs::create_dir_all(&dir).unwrap();
        let process = Process {
            pid: 10,
            role: DAEMON,
            started: 1,
        };
        assert!(!runs_this_binary(&proc, &process, &own_exe));
        std::os::unix::fs::symlink(&own_exe, dir.join("exe")).unwrap();
        assert!(runs_this_binary(&proc, &process, &own_exe));
        assert!(!runs_this_binary(&proc, &process, &proc.join("missing")));
    }

    #[test]
    fn the_daemon_reports_its_version_over_the_socket() {
        let dir = TestDir::new("keymux-test-version-ipc");
        let path = dir.join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let request = ipc::read_request(&mut conn).unwrap();
            assert!(matches!(request, IpcRequest::Version));
            // What the daemon's socket thread does with it
            ipc::reply(&mut conn, &ipc::local_reply(&request).unwrap()).unwrap();
        });
        assert_eq!(
            daemon_version(&path, Duration::from_secs(5)).as_deref(),
            Some(VERSION)
        );
        server.join().unwrap();
    }

    /// A daemon from before the request existed cannot decode it and drops the connection,
    /// exactly what the accept loop in `daemon.rs` does on a failed decode.
    #[test]
    fn an_older_daemon_that_hangs_up_gives_no_version_at_once() {
        let dir = TestDir::new("keymux-test-version-old");
        let path = dir.join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let _ = ipc::read_request(&mut conn);
        });
        let started = std::time::Instant::now();
        assert_eq!(daemon_version(&path, Duration::from_secs(5)), None);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a hang-up must not wait for the timeout: {:?}",
            started.elapsed()
        );
        server.join().unwrap();
    }

    #[test]
    fn a_daemon_that_never_answers_costs_only_the_timeout() {
        let dir = TestDir::new("keymux-test-version-silent");
        let path = dir.join("s.sock");
        // Bound but never accepted: the connection is made, the answer never comes
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let started = std::time::Instant::now();
        assert_eq!(daemon_version(&path, Duration::from_millis(25)), None);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        drop(listener);
        // No socket, and a socket file nobody listens on
        assert_eq!(daemon_version(&path, Duration::from_millis(25)), None);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(daemon_version(&path, Duration::from_millis(25)), None);
    }

    #[test]
    fn an_answer_that_is_not_a_version_is_no_version() {
        let dir = TestDir::new("keymux-test-version-wrong");
        let path = dir.join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().unwrap();
            let _ = ipc::read_request(&mut conn);
            ipc::reply(&mut conn, &IpcResponse::Pong).unwrap();
        });
        assert_eq!(daemon_version(&path, Duration::from_secs(5)), None);
        server.join().unwrap();
    }

    /// Old daemons decode requests by position, so the new one must be added at the end.
    #[test]
    fn the_version_request_does_not_renumber_the_older_ones() {
        let tag = |r: &IpcRequest| bincode::serialize(r).unwrap()[..4].to_vec();
        assert_eq!(tag(&IpcRequest::Ping), 0u32.to_le_bytes());
        assert_eq!(tag(&IpcRequest::SubscribeOverlay), 15u32.to_le_bytes());
        assert_eq!(tag(&IpcRequest::Version), 16u32.to_le_bytes());
        let tag = |r: &IpcResponse| bincode::serialize(r).unwrap()[..4].to_vec();
        assert_eq!(tag(&IpcResponse::Pong), 0u32.to_le_bytes());
        assert_eq!(
            tag(&IpcResponse::Version(String::new())),
            7u32.to_le_bytes()
        );
    }
}
