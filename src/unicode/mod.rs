//! Typing characters the virtual keyboard has no key for (`—`, `é`, `Ī`, `™`).
//!
//! The daemon types through a `uinput` keyboard, which has keys, not characters: what a key
//! types is up to the keymap of the session, and the US layout has no key for `—`. So text
//! that has such a character is handed, whole, to a helper in the user's session, which types
//! it with the Wayland virtual-keyboard protocol: it uploads a small keymap that gives spare
//! keycodes the characters it needs, and presses them (as `wtype` does). The helper runs in
//! `keymux overlay`, which is in the session anyway; see [`wayland`].
//!
//! The daemon keeps one helper connection per user ([`Helpers`]), sends it a [`TypeJob`] and
//! waits for the [`Typed`] answer, which the helper sends only once the compositor has handled
//! every key of it. Output after the job therefore comes after it. Output before it is given
//! time to be read first: see `output::retype`.

#[cfg(feature = "overlay")]
pub mod wayland;

use crate::lock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tracing::{info, warn};

/// One piece of output for the helper: delete `backspaces` characters before the cursor, then
/// type `text`. Both in one job, so their order can't be broken.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeJob {
    pub backspaces: u32,
    pub text: String,
}

/// The helper's answer to a [`TypeJob`]: `true` once the compositor has handled every key of
/// it, `false` when it typed nothing at all (it lost its Wayland connection, say).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Typed(pub bool);

/// Why a helper did not type a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// No helper is connected for the user
    NoHelper,
    /// The helper typed nothing: the text can still be typed another way
    NothingTyped,
    /// The helper went away or stopped answering mid-job: some of it may be typed, so typing
    /// it again another way could double it
    Unknown,
}

/// Something that types a job for the output path: the user's helper, or a fake in a test.
pub trait Helper {
    /// Delete `backspaces` characters, type `text`, and return once both are done.
    ///
    /// # Errors
    /// Why it was not typed, see [`Failure`].
    fn type_text(&self, backspaces: usize, text: &str) -> Result<(), Failure>;
}

/// The helper of the user with this uid, as registered with [`Helpers::global`].
#[derive(Debug, Clone, Copy)]
pub struct UserHelper(pub u32);

impl Helper for UserHelper {
    fn type_text(&self, backspaces: usize, text: &str) -> Result<(), Failure> {
        Helpers::global().type_text(self.0, backspaces, text)
    }
}

/// How long the daemon waits to hand a job over. Writing to a live helper never waits.
const SEND_TIMEOUT: Duration = Duration::from_millis(200);
/// How long the daemon waits for a job to be typed: a keymap upload and one round trip to
/// the compositor, normally a few milliseconds.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(2);

/// The daemon's helper connections, one per user. A user can only register a helper for
/// themselves: the connection's peer uid decides whose it is.
#[derive(Default)]
pub struct Helpers {
    by_user: Mutex<HashMap<u32, Arc<Mutex<UnixStream>>>>,
}

impl Helpers {
    /// The daemon's one set of helpers.
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<Helpers> = OnceLock::new();
        GLOBAL.get_or_init(Self::default)
    }

    /// Take `stream` (a connection that sent `ServeUnicode`) as the helper of the user that
    /// opened it, in place of any earlier one.
    pub fn register(&self, stream: UnixStream) {
        let Some(uid) = peer_uid(&stream) else {
            warn!("unicode helper: cannot tell whose connection it is, ignoring it");
            return;
        };
        if stream.set_write_timeout(Some(SEND_TIMEOUT)).is_err()
            || stream.set_read_timeout(Some(ANSWER_TIMEOUT)).is_err()
        {
            return;
        }
        info!("unicode helper connected for user {uid}");
        lock(&self.by_user).insert(uid, Arc::new(Mutex::new(stream)));
        // A later loss deserves its own warning
        WARNED.store(false, Ordering::Relaxed);
    }

    /// Whether a helper is connected for `uid`.
    pub fn connected(&self, uid: u32) -> bool {
        lock(&self.by_user).contains_key(&uid)
    }

    /// Have the helper of `uid` delete `backspaces` characters and type `text`; returns once
    /// it has.
    ///
    /// # Errors
    /// Why it was not typed, see [`Failure`]. A helper that failed is forgotten.
    pub fn type_text(&self, uid: u32, backspaces: usize, text: &str) -> Result<(), Failure> {
        let Some(helper) = lock(&self.by_user).get(&uid).cloned() else {
            return Err(Failure::NoHelper);
        };
        let job = TypeJob {
            backspaces: u32::try_from(backspaces).unwrap_or(u32::MAX),
            text: text.to_string(),
        };
        let result = {
            let mut stream = lock(&helper);
            exchange(&mut *stream, &job)
        };
        match result {
            Ok(()) => return Ok(()),
            Err(Failure::NothingTyped) => warn!("unicode helper could not type, falling back"),
            Err(_) => warn!("unicode helper went away mid-text; some of it may be missing"),
        }
        // Forget it, unless another has taken its place meanwhile
        let mut by_user = lock(&self.by_user);
        if by_user
            .get(&uid)
            .is_some_and(|now| Arc::ptr_eq(now, &helper))
        {
            by_user.remove(&uid);
        }
        result
    }
}

/// Send `job` on a helper connection and wait for its answer.
fn exchange<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    job: &TypeJob,
) -> Result<(), Failure> {
    // Nothing sent: nothing typed
    crate::ipc::write_frame(stream, job).map_err(|_| Failure::NothingTyped)?;
    match crate::ipc::read_frame::<Typed>(stream) {
        Some(Typed(true)) => Ok(()),
        Some(Typed(false)) => Err(Failure::NothingTyped),
        None => Err(Failure::Unknown),
    }
}

/// The uid of the process at the other end of `stream`.
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>()).ok()?;
    // SAFETY: `cred` is a valid ucred and `len` its size, as SO_PEERCRED requires
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            std::ptr::addr_of_mut!(len),
        )
    };
    (status == 0).then_some(cred.uid)
}

/// Whether the "characters typed as `?`" warning was given since the last helper connected.
static WARNED: AtomicBool = AtomicBool::new(false);

/// Warn once (until a helper connects again) that characters with no key are being typed as
/// `?`, and why.
pub fn warn_unavailable(why: &str) {
    if !WARNED.swap(true, Ordering::Relaxed) {
        warn!(
            "typing a character with no key as '?': {why}. Run `keymux overlay` in your \
             Wayland session (systemctl --user enable --now keymux-steno), or set \
             unicode_output: CtrlShiftU for GTK/IBus apps. Warned once."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_and_its_answer_cross_the_wire() {
        let (mut daemon, mut helper) = UnixStream::pair().unwrap();
        let job = TypeJob {
            backspaces: 3,
            text: "NĪT — ok".into(),
        };
        let typer = std::thread::spawn(move || {
            let got: TypeJob = crate::ipc::read_frame(&mut helper).unwrap();
            crate::ipc::write_frame(&mut helper, &Typed(true)).unwrap();
            got
        });
        assert_eq!(exchange(&mut daemon, &job), Ok(()));
        assert_eq!(typer.join().unwrap(), job);
    }

    #[test]
    fn a_helper_that_types_nothing_or_hangs_up_is_told_apart() {
        let (mut daemon, mut helper) = UnixStream::pair().unwrap();
        let typer = std::thread::spawn(move || {
            let _: TypeJob = crate::ipc::read_frame(&mut helper).unwrap();
            crate::ipc::write_frame(&mut helper, &Typed(false)).unwrap();
            let _: TypeJob = crate::ipc::read_frame(&mut helper).unwrap();
            // Hangs up without answering: the job may be half typed
        });
        let job = TypeJob {
            backspaces: 0,
            text: "é".into(),
        };
        assert_eq!(exchange(&mut daemon, &job), Err(Failure::NothingTyped));
        assert_eq!(exchange(&mut daemon, &job), Err(Failure::Unknown));
        typer.join().unwrap();
        // Gone before the job is sent: nothing typed, so it can go another way
        assert_eq!(exchange(&mut daemon, &job), Err(Failure::NothingTyped));
    }

    #[test]
    fn a_registered_helper_belongs_to_the_user_that_connected_it() {
        let helpers = Helpers::default();
        let (daemon, helper) = UnixStream::pair().unwrap();
        let me = unsafe { libc::getuid() };
        assert_eq!(peer_uid(&daemon), Some(me));
        assert_eq!(
            helpers.type_text(me, 0, "é"),
            Err(Failure::NoHelper),
            "none yet"
        );
        helpers.register(daemon);
        assert!(helpers.connected(me));
        assert!(!helpers.connected(me + 1), "only for its own user");
        drop(helper);
        // Its helper is gone: the job fails without typing and the helper is forgotten
        assert_eq!(helpers.type_text(me, 0, "é"), Err(Failure::NothingTyped));
        assert!(!helpers.connected(me));
    }
}
