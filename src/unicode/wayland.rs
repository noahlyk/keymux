//! The Unicode helper: types the daemon's [`TypeJob`]s with the Wayland virtual-keyboard
//! protocol (`zwp_virtual_keyboard_v1`), from inside the user's session.
//!
//! A virtual keyboard brings its own keymap, so it can type any character: the helper gives
//! each character it needs a keycode of its own in a small keymap, uploads that keymap, and
//! presses the keycodes, as `wtype` does. The keymap grows as new characters come and is
//! uploaded again only when one is missing, so typing the same characters again costs no
//! upload. The compositor sends the app the helper's keymap with its keys and the real
//! keyboard's keymap again with the next real key, so the user's own layout is untouched.
//!
//! It runs on its own thread and its own Wayland connection inside `keymux overlay`, so
//! drawing never holds up typing and typing never holds up drawing.

use super::{TypeJob, Typed};
use anyhow::{Context, Result};
use smithay_client_toolkit::reexports::client::globals::{registry_queue_init, GlobalListContents};
use smithay_client_toolkit::reexports::client::protocol::{wl_registry, wl_seat};
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, EventQueue, QueueHandle};
use std::io::Write;
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;

/// Stands for Backspace in a keymap: a control character no text contains as itself.
const BACKSPACE: char = '\u{8}';

/// The first keycode a keymap gives out (xkb numbering: the evdev code plus 8).
const FIRST_KEYCODE: u32 = 9;

/// The most characters one keymap holds. X11 apps (through Xwayland) see keycodes up to 255
/// only, so the keymap stays below that.
const CAPACITY: usize = 200;

/// The name xkb knows `ch`'s keysym by, or `None` for a control character with no key.
fn keysym_name(ch: char) -> Option<String> {
    match ch {
        BACKSPACE => Some("BackSpace".into()),
        '\n' => Some("Return".into()),
        '\t' => Some("Tab".into()),
        // xkb maps `U` and a code point to the character's keysym, Latin-1 included
        ch if ch < ' ' || ('\u{7f}'..'\u{a0}').contains(&ch) => None,
        ch => Some(format!("U{:04X}", u32::from(ch))),
    }
}

/// The characters of the keymap the compositor has, in keycode order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Keymap {
    chars: Vec<char>,
}

impl Keymap {
    /// The evdev keycode that types `ch`, if the keymap has it.
    fn code_of(&self, ch: char) -> Option<u32> {
        let index = self.chars.iter().position(|&c| c == ch)?;
        Some(FIRST_KEYCODE + u32::try_from(index).ok()? - 8)
    }

    /// The keymap in xkb's text format.
    fn text(&self) -> String {
        let mut codes = String::new();
        let mut symbols = String::new();
        for (index, &ch) in self.chars.iter().enumerate() {
            let code = FIRST_KEYCODE as usize + index;
            let name = keysym_name(ch).unwrap_or_else(|| "NoSymbol".into());
            codes.push_str(&format!("<K{code}> = {code};\n"));
            symbols.push_str(&format!("key <K{code}> {{[{name}]}};\n"));
        }
        let maximum = (FIRST_KEYCODE as usize + self.chars.len()).max(FIRST_KEYCODE as usize);
        format!(
            "xkb_keymap {{\n\
             xkb_keycodes \"keymux\" {{\nminimum = 8;\nmaximum = {maximum};\n{codes}}};\n\
             xkb_types \"keymux\" {{ include \"complete\" }};\n\
             xkb_compatibility \"keymux\" {{ include \"complete\" }};\n\
             xkb_symbols \"keymux\" {{\n{symbols}}};\n\
             }};\n"
        )
    }
}

/// What to send the compositor for a job, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    /// Upload this keymap
    Keymap(String),
    /// Press and release this evdev keycode
    Tap(u32),
}

/// The steps that delete `backspaces` characters and type `text` with `keymap`, which they
/// grow (or replace, once it is full) as characters it lacks come up. A control character
/// with no key is left out.
fn steps(keymap: &mut Keymap, backspaces: usize, text: &str) -> Vec<Step> {
    let keys: Vec<char> = std::iter::repeat_n(BACKSPACE, backspaces)
        .chain(text.chars().filter(|&ch| keysym_name(ch).is_some()))
        .collect();
    let mut steps = Vec::with_capacity(keys.len() + 1);
    // Everything the job lacks goes up in one upload, if it fits
    let mut missing: Vec<char> = Vec::new();
    for &ch in &keys {
        if keymap.code_of(ch).is_none() && !missing.contains(&ch) {
            missing.push(ch);
        }
    }
    if !missing.is_empty() && keymap.chars.len() + missing.len() <= CAPACITY {
        keymap.chars.extend(missing);
        steps.push(Step::Keymap(keymap.text()));
    }
    for (at, &ch) in keys.iter().enumerate() {
        if keymap.code_of(ch).is_none() {
            // Full: start a new keymap with the characters still to come
            keymap.chars.clear();
            for &next in &keys[at..] {
                if keymap.chars.len() == CAPACITY {
                    break;
                }
                if !keymap.chars.contains(&next) {
                    keymap.chars.push(next);
                }
            }
            steps.push(Step::Keymap(keymap.text()));
        }
        if let Some(code) = keymap.code_of(ch) {
            steps.push(Step::Tap(code));
        }
    }
    steps
}

/// Nothing the helper binds sends an event it needs.
struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardManagerV1,
        _: <ZwpVirtualKeyboardManagerV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: <ZwpVirtualKeyboardV1 as smithay_client_toolkit::reexports::client::Proxy>::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// A virtual keyboard on the session's compositor.
pub struct Typer {
    queue: EventQueue<State>,
    keyboard: ZwpVirtualKeyboardV1,
    keymap: Keymap,
    started: Instant,
}

impl Typer {
    /// Connect to the session's compositor and make a virtual keyboard on its first seat.
    ///
    /// # Errors
    /// No Wayland session, or a compositor without the virtual-keyboard protocol.
    pub fn connect() -> Result<Self> {
        let conn = Connection::connect_to_env().context("no Wayland session")?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).context("reading the compositor's globals")?;
        let handle = queue.handle();
        let seat: wl_seat::WlSeat = globals
            .bind(&handle, 1..=1, ())
            .context("the compositor has no seat")?;
        let manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&handle, 1..=1, ())
            .context("the compositor has no virtual-keyboard protocol (zwp_virtual_keyboard_v1)")?;
        let keyboard = manager.create_virtual_keyboard(&seat, &handle, ());
        queue
            .roundtrip(&mut State)
            .context("making a virtual keyboard")?;
        Ok(Self {
            queue,
            keyboard,
            keymap: Keymap::default(),
            started: Instant::now(),
        })
    }

    /// Delete `backspaces` characters and type `text`, and return once the compositor has
    /// handled every key.
    ///
    /// # Errors
    /// The Wayland connection failed. The keymap is then forgotten, so a later job uploads it
    /// again.
    pub fn type_text(&mut self, backspaces: usize, text: &str) -> Result<()> {
        let result = self.send(backspaces, text);
        if result.is_err() {
            self.keymap = Keymap::default();
        }
        result
    }

    fn send(&mut self, backspaces: usize, text: &str) -> Result<()> {
        for step in steps(&mut self.keymap, backspaces, text) {
            match step {
                Step::Keymap(text) => {
                    let (fd, size) = keymap_file(&text)?;
                    // 1: XKB_V1, the only format there is
                    self.keyboard.keymap(1, fd.as_fd(), size);
                    self.keyboard.modifiers(0, 0, 0, 0);
                }
                Step::Tap(code) => {
                    let time = self.time();
                    self.keyboard.key(time, code, 1);
                    self.keyboard.key(time, code, 0);
                }
            }
        }
        // Done means the compositor has handled every request before this one
        self.queue
            .roundtrip(&mut State)
            .context("waiting for the compositor")?;
        Ok(())
    }

    /// Milliseconds since the keyboard was made, as the protocol's key timestamps.
    fn time(&self) -> u32 {
        u32::try_from(self.started.elapsed().as_millis() % (1 << 32)).unwrap_or(0)
    }

    /// Upload the keymap `text` without pressing anything, and wait for the compositor to take
    /// it: checks that it accepts the keymap, with nothing typed.
    #[cfg(test)]
    fn upload_only(&mut self, text: &str) -> Result<()> {
        let (fd, size) = keymap_file(text)?;
        self.keyboard.keymap(1, fd.as_fd(), size);
        self.queue
            .roundtrip(&mut State)
            .context("waiting for the compositor")?;
        Ok(())
    }
}

/// A sealed-off in-memory file holding `text` and a terminating NUL, as the protocol wants it,
/// and its size.
fn keymap_file(text: &str) -> Result<(OwnedFd, u32)> {
    // SAFETY: a valid NUL-terminated name and flags; the result is checked before use
    let raw = unsafe { libc::memfd_create(c"keymux-keymap".as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(std::io::Error::last_os_error()).context("making the keymap file");
    }
    // SAFETY: `raw` is a new descriptor that nothing else owns
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut file = std::fs::File::from(fd);
    file.write_all(text.as_bytes())?;
    file.write_all(&[0])?;
    let size = u32::try_from(text.len() + 1).context("keymap too big")?;
    Ok((OwnedFd::from(file), size))
}

/// Type the daemon's jobs from `stream` until the daemon goes away (`Ok`) or the Wayland
/// connection fails (`Err`, after answering the job it failed on).
///
/// # Errors
/// The Wayland connection failed.
pub fn serve(mut stream: UnixStream, typer: &mut Typer) -> Result<()> {
    while let Some(job) = crate::ipc::read_frame::<TypeJob>(&mut stream) {
        let typed = typer.type_text(job.backspaces as usize, &job.text);
        let answer = Typed(typed.is_ok());
        if crate::ipc::write_frame(&mut stream, &answer).is_err() {
            return Ok(());
        }
        typed?;
    }
    Ok(())
}

const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);

/// Serve the daemon on a thread of its own, for as long as the process runs.
///
/// It offers to type, types what comes, and offers again when the daemon is back. Without a
/// compositor that has
/// the virtual-keyboard protocol it logs why and stops, and the daemon types such characters
/// as its `unicode_output` setting says.
pub fn spawn() {
    let spawned = std::thread::Builder::new()
        .name("unicode-helper".into())
        .spawn(|| {
            if let Err(e) = run(false) {
                tracing::warn!("unicode helper stopped: {e:#}");
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("unicode helper not started: {e}");
    }
}

/// Serve the daemon, until the Wayland connection fails.
///
/// With `once`, return as soon as the daemon goes away, after it has been connected (the
/// caller exits so the init system restarts it with the daemon); otherwise keep offering.
/// Returns `Ok` at once when there is no virtual keyboard to type with.
///
/// # Errors
/// The daemon went away (`once`), or the Wayland connection failed.
pub fn run(once: bool) -> Result<()> {
    let mut typer = match Typer::connect() {
        Ok(typer) => typer,
        Err(e) => {
            tracing::info!("no unicode helper: {e:#}");
            return Ok(());
        }
    };
    let mut retry = RETRY_MIN;
    loop {
        match crate::ipc::serve_unicode() {
            Ok(stream) => {
                tracing::debug!("unicode helper offered to the daemon");
                serve(stream, &mut typer)?;
                // A daemon still answering dropped the offer: one from before helpers existed.
                // Keep offering, slowly, rather than exit into a restart loop.
                if crate::ipc::daemon_alive() {
                    retry = RETRY_MAX;
                } else if once {
                    anyhow::bail!("the keymux daemon went away");
                } else {
                    retry = RETRY_MIN;
                }
            }
            Err(e) => tracing::debug!("unicode helper: {e:#}"),
        }
        std::thread::sleep(retry);
        retry = (retry * 2).min(RETRY_MAX);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taps(steps: &[Step]) -> Vec<u32> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Tap(code) => Some(*code),
                Step::Keymap(_) => None,
            })
            .collect()
    }

    fn uploads(steps: &[Step]) -> usize {
        steps
            .iter()
            .filter(|step| matches!(step, Step::Keymap(_)))
            .count()
    }

    #[test]
    fn characters_get_keysyms_and_control_characters_none() {
        assert_eq!(keysym_name('—').as_deref(), Some("U2014"));
        assert_eq!(keysym_name('Ī').as_deref(), Some("U012A"));
        assert_eq!(keysym_name('é').as_deref(), Some("U00E9"));
        assert_eq!(keysym_name('a').as_deref(), Some("U0061"));
        assert_eq!(keysym_name('😀').as_deref(), Some("U1F600"));
        assert_eq!(keysym_name('\n').as_deref(), Some("Return"));
        assert_eq!(keysym_name('\t').as_deref(), Some("Tab"));
        assert_eq!(keysym_name(BACKSPACE).as_deref(), Some("BackSpace"));
        assert_eq!(keysym_name('\r'), None);
        assert_eq!(keysym_name('\u{85}'), None);
    }

    #[test]
    fn a_job_uploads_its_characters_once_then_taps_backspaces_first() {
        let mut keymap = Keymap::default();
        let steps = steps(&mut keymap, 2, "NĪT");
        assert!(matches!(steps[0], Step::Keymap(_)));
        assert_eq!(uploads(&steps), 1);
        // Backspace is keycode 1, then N, Ī, T in order of first use
        assert_eq!(taps(&steps), vec![1, 1, 2, 3, 4]);
        assert_eq!(keymap.chars, vec![BACKSPACE, 'N', 'Ī', 'T']);
    }

    #[test]
    fn characters_already_uploaded_cost_no_upload() {
        let mut keymap = Keymap::default();
        steps(&mut keymap, 0, "ĀĒ");
        let again = steps(&mut keymap, 0, "ĒĀĒ");
        assert_eq!(uploads(&again), 0);
        assert_eq!(taps(&again), vec![2, 1, 2]);
        let more = steps(&mut keymap, 0, "Ā—");
        assert_eq!(uploads(&more), 1, "only a new character uploads");
        assert_eq!(
            taps(&more),
            vec![1, 3],
            "and the old ones keep their keycodes"
        );
    }

    #[test]
    fn a_full_keymap_starts_over_and_one_job_bigger_than_a_keymap_still_types_in_order() {
        let mut keymap = Keymap::default();
        let many: String = (0..CAPACITY + 50)
            .map(|i| char::from_u32(0x4e00 + u32::try_from(i).unwrap()).unwrap())
            .collect();
        let steps = steps(&mut keymap, 0, &many);
        assert_eq!(uploads(&steps), 2);
        let codes = taps(&steps);
        assert_eq!(codes.len(), CAPACITY + 50, "every character typed");
        // The second keymap starts over at the first keycode, with what was still to come
        assert_eq!(codes[CAPACITY], 1);
        assert!(keymap.chars.len() <= CAPACITY);
        assert_eq!(keymap.chars[0], many.chars().nth(CAPACITY).unwrap());
    }

    #[test]
    fn the_keymap_text_names_each_keycode_and_its_keysym() {
        let mut keymap = Keymap::default();
        steps(&mut keymap, 1, "—\n");
        let text = keymap.text();
        assert!(text.contains("<K9> = 9;"));
        assert!(text.contains("key <K9> {[BackSpace]};"));
        assert!(text.contains("key <K10> {[U2014]};"));
        assert!(text.contains("key <K11> {[Return]};"));
        assert!(text.contains("maximum = 12;"));
    }

    #[test]
    fn a_control_character_with_no_key_is_left_out() {
        let mut keymap = Keymap::default();
        let steps = steps(&mut keymap, 0, "a\rb");
        assert_eq!(taps(&steps).len(), 2);
    }

    /// Live, and safe: connects to the session's compositor, makes a virtual keyboard and
    /// uploads a keymap with `—`, `Ī` and `™`, but presses nothing. Passes when the
    /// compositor has the protocol and takes the keymap without a protocol error.
    /// `cargo test --lib unicode::wayland -- --ignored compositor_takes`
    #[test]
    #[ignore = "needs a Wayland session"]
    fn compositor_takes_a_keymap_without_typing() {
        let mut typer =
            Typer::connect().expect("a Wayland compositor with zwp_virtual_keyboard_v1");
        let mut keymap = Keymap::default();
        steps(&mut keymap, 1, "NĪT — ™ °³²");
        typer.upload_only(&keymap.text()).expect("keymap accepted");
    }

    /// Live, and it TYPES into the focused window: run it with a scratch editor focused.
    /// After a 3 s pause it types `NĪT -> night`, then deletes `night` with five backspaces and
    /// types `day — ™ °³²`: the window should end with `NĪT -> day — ™ °³²`.
    /// `cargo test --lib unicode::wayland -- --ignored types_into`
    #[test]
    #[ignore = "types into the focused window"]
    fn types_into_the_focused_window() {
        let mut typer =
            Typer::connect().expect("a Wayland compositor with zwp_virtual_keyboard_v1");
        std::thread::sleep(Duration::from_secs(3));
        typer.type_text(0, "NĪT -> night").unwrap();
        typer.type_text(5, "day — ™ °³²").unwrap();
    }
}
