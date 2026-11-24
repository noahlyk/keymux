//! The output side of a keyboard: what the keymap decided, as events on the virtual keyboard,
//! paced so that nothing the kernel queues for a slow reader is ever thrown away.

use super::{KeymapProcessor, ProcessResult};
use crate::config::UnicodeOutput;
use crate::keycode::KeyCode;
use crate::steno::translate::OutputOp;
use crate::unicode::{self, Failure, Helper};
use evdev::uinput::VirtualDevice;
use evdev::{EventType, InputEvent, Key};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tracing::{error, info};

const SYN_REPORT: i32 = 0;
const SYN_CODE: u16 = 0;

/// Where emitted events go: the virtual keyboard, or a recorder in a test.
pub trait Sink {
    fn emit(&mut self, events: &[InputEvent]) -> io::Result<()>;
}

impl Sink for VirtualDevice {
    fn emit(&mut self, events: &[InputEvent]) -> io::Result<()> {
        Self::emit(self, events)
    }
}

/// How text with a character the virtual keyboard has no key for is typed: the user's
/// `unicode_output` setting, and their helper if the daemon has one.
#[derive(Clone, Copy, Default)]
pub struct Unicode<'a> {
    pub mode: UnicodeOutput,
    pub helper: Option<&'a dyn Helper>,
}

/// Send what the keymap decided.
pub fn emit_result<S: Sink + ?Sized>(
    sink: &mut S,
    pacer: &mut Pacer,
    unicode: Unicode<'_>,
    result: ProcessResult,
) -> io::Result<()> {
    match result {
        // One event of an ordinary key: straight out, nothing to pace
        ProcessResult::EmitKey(key, pressed) => sink.emit(&[key_event(key, pressed)]),
        ProcessResult::MultipleEvents(events) => emit_key_events(sink, pacer, &events),
        ProcessResult::TypeString(text) => retype(sink, pacer, unicode, 0, &text),
        ProcessResult::Retype { backspaces, text } => {
            retype(sink, pacer, unicode, backspaces, &text)
        }
        ProcessResult::Ops(ops) => emit_ops(sink, pacer, unicode, &ops),
        ProcessResult::None => Ok(()),
    }
}

/// Delete `backspaces` characters before the cursor, then type `text`.
///
/// Text whose characters all have a key goes out as keys, as it always has. Text with one
/// that has none (`—`, `Ī`) goes as `unicode.mode` says; through the helper, the whole of it,
/// backspaces included, is one job, so nothing in it can be reordered. A backspace deletes one
/// character (`char`) however it was typed, which is what the steno engine counts in.
fn retype<S: Sink + ?Sized>(
    sink: &mut S,
    pacer: &mut Pacer,
    unicode: Unicode<'_>,
    backspaces: usize,
    text: &str,
) -> io::Result<()> {
    if text.chars().all(has_key) {
        return type_taps(sink, pacer, &retype_taps(backspaces, text));
    }
    match unicode.mode {
        UnicodeOutput::Auto | UnicodeOutput::Wayland => {
            let why = match unicode.helper {
                None => "no helper",
                Some(helper) => {
                    // The helper's keys reach the compositor by another road than ours, so
                    // ours go first: wait until what was sent can have been read
                    pacer.wait_until_read();
                    match helper.type_text(backspaces, text) {
                        Ok(()) => return Ok(()),
                        Err(Failure::NoHelper) => "no helper is connected",
                        Err(Failure::NothingTyped) => "the helper could not type it",
                        // Part of it may be on screen: typing it again could double it
                        Err(Failure::Unknown) => {
                            error!(
                                "unicode helper failed mid-text; {} characters may be missing",
                                text.chars().count()
                            );
                            return Ok(());
                        }
                    }
                }
            };
            unicode::warn_unavailable(why);
            type_taps(sink, pacer, &retype_taps(backspaces, &placeholders(text)))
        }
        UnicodeOutput::CtrlShiftU => {
            emit_keystrokes(sink, pacer, &ctrl_shift_u_keystrokes(backspaces, text))
        }
        UnicodeOutput::Off => {
            static SAID: AtomicBool = AtomicBool::new(false);
            if !SAID.swap(true, Ordering::Relaxed) {
                info!("unicode_output is Off: characters with no key are typed as '?'");
            }
            type_taps(sink, pacer, &retype_taps(backspaces, &placeholders(text)))
        }
    }
}

/// Send taps as the pacer allows.
fn type_taps<S: Sink + ?Sized>(sink: &mut S, pacer: &mut Pacer, taps: &[Tap]) -> io::Result<()> {
    for (at, range) in pacer.schedule_taps(Instant::now(), taps) {
        if let Some(wait) = at.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        sink.emit(&tap_events(&taps[range]))?;
    }
    Ok(())
}

/// What is typed in place of a character with no key when it cannot be typed: one character,
/// so that undo still deletes as many as the steno engine counts.
pub const PLACEHOLDER: char = '?';

/// `text` with every character that has no key replaced by [`PLACEHOLDER`].
pub fn placeholders(text: &str) -> String {
    text.chars()
        .map(|ch| if has_key(ch) { ch } else { PLACEHOLDER })
        .collect()
}

/// Whether the virtual keyboard has a key for `ch` (on the US layout).
pub const fn has_key(ch: char) -> bool {
    char_to_key(ch).0.is_some()
}

/// A stretch of text that is typed one way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Run<'t> {
    /// Characters that all have a key
    Keys(&'t str),
    /// Characters none of which has a key
    Unicode(&'t str),
}

/// `text` cut into the longest runs of characters with a key and without one, in order.
pub fn runs(text: &str) -> Vec<Run<'_>> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut keyed = None;
    for (at, ch) in text.char_indices() {
        let this = has_key(ch);
        if keyed.is_some_and(|run| run != this) {
            runs.push(run_of(&text[start..at], keyed == Some(true)));
            start = at;
        }
        keyed = Some(this);
    }
    if start < text.len() {
        runs.push(run_of(&text[start..], keyed == Some(true)));
    }
    runs
}

const fn run_of(text: &str, keyed: bool) -> Run<'_> {
    if keyed {
        Run::Keys(text)
    } else {
        Run::Unicode(text)
    }
}

/// The keystrokes that delete `backspaces` characters then type `text`, a character with no
/// key as GTK and IBus take one: Ctrl+Shift+U, its code point in hex, Space.
pub fn ctrl_shift_u_keystrokes(backspaces: usize, text: &str) -> Vec<Vec<InputEvent>> {
    let key = |key: Key, pressed: bool| {
        [
            InputEvent::new_now(EventType::KEY, key.code(), i32::from(pressed)),
            syn_report(),
        ]
    };
    let taps_of = |taps: Vec<Tap>| -> Vec<Vec<InputEvent>> {
        taps.iter()
            .map(|tap| tap_events(std::slice::from_ref(tap)))
            .collect()
    };
    let mut keystrokes = taps_of(retype_taps(backspaces, ""));
    for run in runs(text) {
        match run {
            Run::Keys(keys) => keystrokes.extend(taps_of(retype_taps(0, keys))),
            Run::Unicode(chars) => {
                for ch in chars.chars() {
                    let mut start = Vec::with_capacity(12);
                    start.extend(key(Key::KEY_LEFTCTRL, true));
                    start.extend(key(Key::KEY_LEFTSHIFT, true));
                    start.extend(key(Key::KEY_U, true));
                    start.extend(key(Key::KEY_U, false));
                    start.extend(key(Key::KEY_LEFTSHIFT, false));
                    start.extend(key(Key::KEY_LEFTCTRL, false));
                    keystrokes.push(start);
                    let hex = format!("{:x} ", u32::from(ch));
                    keystrokes.extend(taps_of(retype_taps(0, &hex)));
                }
            }
        }
    }
    keystrokes
}

/// Do what a steno stroke asked for, in order: delete, type, press keys.
fn emit_ops<S: Sink + ?Sized>(
    sink: &mut S,
    pacer: &mut Pacer,
    unicode: Unicode<'_>,
    ops: &[OutputOp],
) -> io::Result<()> {
    let mut i = 0;
    while i < ops.len() {
        match &ops[i] {
            OutputOp::Backspaces(count) => {
                // The text that replaces what was deleted goes out with it
                if let Some(OutputOp::Text(text)) = ops.get(i + 1) {
                    retype(sink, pacer, unicode, *count, text)?;
                    i += 1;
                } else {
                    retype(sink, pacer, unicode, *count, "")?;
                }
            }
            OutputOp::Text(text) => retype(sink, pacer, unicode, 0, text)?,
            OutputOp::Keys(events) => emit_key_events(sink, pacer, events)?,
            // Commands were run by the steno engine
            OutputOp::Command(_) => {}
        }
        i += 1;
    }
    Ok(())
}

/// Press and release keys one event at a time, in order, as the pacer allows.
fn emit_key_events<S: Sink + ?Sized>(
    sink: &mut S,
    pacer: &mut Pacer,
    events: &[(KeyCode, bool)],
) -> io::Result<()> {
    let groups: Vec<Vec<InputEvent>> = events
        .iter()
        .map(|&(key, pressed)| vec![key_event(key, pressed), syn_report()])
        .collect();
    emit_keystrokes(sink, pacer, &groups)
}

/// Emit keystrokes in order, as the pacer allows.
fn emit_keystrokes<S: Sink + ?Sized>(
    sink: &mut S,
    pacer: &mut Pacer,
    keystrokes: &[Vec<InputEvent>],
) -> io::Result<()> {
    let sizes: Vec<usize> = keystrokes.iter().map(Vec::len).collect();
    for (at, range) in pacer.schedule(Instant::now(), &sizes) {
        if let Some(wait) = at.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        sink.emit(&keystrokes[range].concat())?;
    }
    Ok(())
}

/// One key pressed and released, with shift held for it or not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tap {
    key: Key,
    shift: bool,
}

/// What one tap costs in events: its press and release, each with a SYN_REPORT. Holding shift
/// costs the same again, once for a whole run of shifted taps sent together.
const TAP_EVENTS: usize = 4;

/// The taps that delete `backspaces` characters then type `text`.
pub fn retype_taps(backspaces: usize, text: &str) -> Vec<Tap> {
    let delete = Tap {
        key: Key::KEY_BACKSPACE,
        shift: false,
    };
    let typed = text.chars().filter_map(|ch| {
        let (key, shift) = char_to_key(ch);
        key.map(|key| Tap { key, shift })
    });
    std::iter::repeat_n(delete, backspaces)
        .chain(typed)
        .collect()
}

/// The events of taps sent together: each key pressed and released, every event followed by a
/// SYN_REPORT. Shift goes down once before a run of shifted taps and comes up once after it,
/// as a hand types capitals, so a capital costs no more events than a small letter (the kernel's
/// queue is what limits how much goes out at once, see [`SAFE_EVENTS`]). Shift is never left
/// down at the end.
pub fn tap_events(taps: &[Tap]) -> Vec<InputEvent> {
    let key = |key: Key, pressed: bool| {
        [
            InputEvent::new_now(EventType::KEY, key.code(), i32::from(pressed)),
            syn_report(),
        ]
    };
    let mut events = Vec::with_capacity((taps.len() + 2) * TAP_EVENTS);
    let mut held = false;
    for tap in taps {
        if tap.shift != held {
            events.extend(key(Key::KEY_LEFTSHIFT, tap.shift));
            held = tap.shift;
        }
        events.extend(key(tap.key, true));
        events.extend(key(tap.key, false));
    }
    if held {
        events.extend(key(Key::KEY_LEFTSHIFT, false));
    }
    events
}

fn key_event(key: KeyCode, pressed: bool) -> InputEvent {
    InputEvent::new_now(
        EventType::KEY,
        Key::new(key.code()).code(),
        i32::from(pressed),
    )
}

fn syn_report() -> InputEvent {
    InputEvent::new_now(EventType::SYNCHRONIZATION, SYN_CODE, SYN_REPORT)
}

/// The most input events allowed unread at once. The kernel keeps 63 per reader of a
/// keyboard (measured with a test device), and when one more arrives it throws away
/// everything queued for that reader, so letters vanish from the middle of a word. A keystroke
/// is 4 events, so this is 12 of them (a run of capitals shares one press of shift: 11 of
/// them), with room left for keys sent elsewhere (modifiers).
pub const SAFE_EVENTS: usize = 48;

/// How long an event counts as possibly unread. An app or compositor busy for longer than
/// this can still lose events, but nothing sent at once ever can: up to `SAFE_EVENTS` go out
/// together, whatever the delay.
pub const UNREAD_WINDOW: Duration = Duration::from_millis(15);

/// Keeps what is sent to one virtual keyboard from outrunning whoever reads it. Output that fits
/// goes out at once. Only what doesn't fit waits, just until earlier events are old enough
/// to have been read.
#[derive(Default)]
pub struct Pacer {
    /// When events were sent, and how many, for the last `UNREAD_WINDOW`
    sent: std::collections::VecDeque<(Instant, usize)>,
}

impl Pacer {
    /// Plan when to send keystrokes of `sizes` events each, starting at `now`. Each part is
    /// the time to send and the keystrokes to send together. A keystroke is never split
    /// across two parts.
    pub fn schedule(
        &mut self,
        now: Instant,
        sizes: &[usize],
    ) -> Vec<(Instant, std::ops::Range<usize>)> {
        self.plan(now, sizes, |_| 0)
    }

    /// Plan when to send `taps`, starting at `now`: each part is the time to send and the taps
    /// to send together, as [`tap_events`] makes them. A run of shifted taps shares one press
    /// and release of shift, and a part that starts inside a run pays for its own, so shift is
    /// never left held between two parts.
    pub fn schedule_taps(
        &mut self,
        now: Instant,
        taps: &[Tap],
    ) -> Vec<(Instant, std::ops::Range<usize>)> {
        let continues = |i: usize| taps[i].shift && i > 0 && taps[i - 1].shift;
        // Shift is paid for by the tap that starts a run
        let sizes: Vec<usize> = (0..taps.len())
            .map(|i| {
                if taps[i].shift && !continues(i) {
                    TAP_EVENTS * 2
                } else {
                    TAP_EVENTS
                }
            })
            .collect();
        // A part starting inside a run presses and releases shift again
        self.plan(now, &sizes, |i| if continues(i) { TAP_EVENTS } else { 0 })
    }

    /// Sleep until everything sent so far can have been read: [`UNREAD_WINDOW`] after the
    /// last of it. Returns at once when that has passed.
    pub fn wait_until_read(&self) {
        let Some(&(last, _)) = self.sent.back() else {
            return;
        };
        if let Some(wait) = (last + UNREAD_WINDOW).checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
    }

    /// [`Self::schedule`], where a part starting at keystroke `i` costs `opening(i)` events
    /// more than its keystrokes' sizes.
    fn plan(
        &mut self,
        now: Instant,
        sizes: &[usize],
        opening: impl Fn(usize) -> usize,
    ) -> Vec<(Instant, std::ops::Range<usize>)> {
        let mut parts = Vec::new();
        // Never before something already planned, so output stays in order
        let mut at = self.sent.back().map_or(now, |&(time, _)| time.max(now));
        let mut next = 0;
        while next < sizes.len() {
            while self
                .sent
                .front()
                .is_some_and(|&(time, _)| time + UNREAD_WINDOW <= at)
            {
                self.sent.pop_front();
            }
            let in_flight: usize = self.sent.iter().map(|&(_, events)| events).sum();
            let (mut end, mut used) = (next, opening(next));
            while end < sizes.len() && in_flight + used + sizes[end] <= SAFE_EVENTS {
                used += sizes[end];
                end += 1;
            }
            if end == next {
                if let Some(&(time, _)) = self.sent.front() {
                    // Nothing fits yet: wait for the oldest events to age out
                    at = time + UNREAD_WINDOW;
                    continue;
                }
                // A keystroke bigger than the whole budget still has to go
                (end, used) = (next + 1, used + sizes[next]);
            }
            parts.push((at, next..end));
            self.sent.push_back((at, used));
            next = end;
        }
        parts
    }
}

const MODIFIERS: [Key; 8] = [
    Key::KEY_LEFTCTRL,
    Key::KEY_RIGHTCTRL,
    Key::KEY_LEFTSHIFT,
    Key::KEY_RIGHTSHIFT,
    Key::KEY_LEFTALT,
    Key::KEY_RIGHTALT,
    Key::KEY_LEFTMETA,
    Key::KEY_RIGHTMETA,
];

/// Release the keys a keyboard could have been left holding when it was last unplugged, before
/// there is a keymap to ask: the modifiers, the letters, and the usual navigation keys.
pub fn release_all_keys_on_startup<S: Sink + ?Sized>(sink: &mut S) {
    let letters = [
        Key::KEY_A,
        Key::KEY_B,
        Key::KEY_C,
        Key::KEY_D,
        Key::KEY_E,
        Key::KEY_F,
        Key::KEY_G,
        Key::KEY_H,
        Key::KEY_I,
        Key::KEY_J,
        Key::KEY_K,
        Key::KEY_L,
        Key::KEY_M,
        Key::KEY_N,
        Key::KEY_O,
        Key::KEY_P,
        Key::KEY_Q,
        Key::KEY_R,
        Key::KEY_S,
        Key::KEY_T,
        Key::KEY_U,
        Key::KEY_V,
        Key::KEY_W,
        Key::KEY_X,
        Key::KEY_Y,
        Key::KEY_Z,
    ];
    let navigation = [
        Key::KEY_UP,
        Key::KEY_DOWN,
        Key::KEY_LEFT,
        Key::KEY_RIGHT,
        Key::KEY_SPACE,
        Key::KEY_ENTER,
        Key::KEY_TAB,
        Key::KEY_ESC,
    ];
    for key in MODIFIERS.iter().chain(&letters).chain(&navigation) {
        let _ = sink.emit(&[InputEvent::new_now(EventType::KEY, key.code(), 0)]);
    }
    let _ = sink.emit(&[syn_report()]);
}

/// Release everything the keymap is holding down, and the modifiers for good measure, before
/// the keyboard is given back.
pub fn release_all_keys<S: Sink + ?Sized>(sink: &mut S, keymap: &KeymapProcessor) {
    let held_keys = keymap.get_held_keys();
    if !held_keys.is_empty() {
        info!(
            "Gracefully releasing {} held key(s) before shutdown",
            held_keys.len()
        );
        for keycode in held_keys {
            let _ = sink.emit(&[key_event(keycode, false)]);
        }
        let _ = sink.emit(&[syn_report()]);
    }
    for key in &MODIFIERS {
        let _ = sink.emit(&[InputEvent::new_now(EventType::KEY, key.code(), 0)]);
    }
    let _ = sink.emit(&[syn_report()]);
}

/// Convert a character to an evdev Key and whether shift is needed
pub const fn char_to_key(ch: char) -> (Option<Key>, bool) {
    match ch {
        'a' => (Some(Key::KEY_A), false),
        'b' => (Some(Key::KEY_B), false),
        'c' => (Some(Key::KEY_C), false),
        'd' => (Some(Key::KEY_D), false),
        'e' => (Some(Key::KEY_E), false),
        'f' => (Some(Key::KEY_F), false),
        'g' => (Some(Key::KEY_G), false),
        'h' => (Some(Key::KEY_H), false),
        'i' => (Some(Key::KEY_I), false),
        'j' => (Some(Key::KEY_J), false),
        'k' => (Some(Key::KEY_K), false),
        'l' => (Some(Key::KEY_L), false),
        'm' => (Some(Key::KEY_M), false),
        'n' => (Some(Key::KEY_N), false),
        'o' => (Some(Key::KEY_O), false),
        'p' => (Some(Key::KEY_P), false),
        'q' => (Some(Key::KEY_Q), false),
        'r' => (Some(Key::KEY_R), false),
        's' => (Some(Key::KEY_S), false),
        't' => (Some(Key::KEY_T), false),
        'u' => (Some(Key::KEY_U), false),
        'v' => (Some(Key::KEY_V), false),
        'w' => (Some(Key::KEY_W), false),
        'x' => (Some(Key::KEY_X), false),
        'y' => (Some(Key::KEY_Y), false),
        'z' => (Some(Key::KEY_Z), false),

        'A' => (Some(Key::KEY_A), true),
        'B' => (Some(Key::KEY_B), true),
        'C' => (Some(Key::KEY_C), true),
        'D' => (Some(Key::KEY_D), true),
        'E' => (Some(Key::KEY_E), true),
        'F' => (Some(Key::KEY_F), true),
        'G' => (Some(Key::KEY_G), true),
        'H' => (Some(Key::KEY_H), true),
        'I' => (Some(Key::KEY_I), true),
        'J' => (Some(Key::KEY_J), true),
        'K' => (Some(Key::KEY_K), true),
        'L' => (Some(Key::KEY_L), true),
        'M' => (Some(Key::KEY_M), true),
        'N' => (Some(Key::KEY_N), true),
        'O' => (Some(Key::KEY_O), true),
        'P' => (Some(Key::KEY_P), true),
        'Q' => (Some(Key::KEY_Q), true),
        'R' => (Some(Key::KEY_R), true),
        'S' => (Some(Key::KEY_S), true),
        'T' => (Some(Key::KEY_T), true),
        'U' => (Some(Key::KEY_U), true),
        'V' => (Some(Key::KEY_V), true),
        'W' => (Some(Key::KEY_W), true),
        'X' => (Some(Key::KEY_X), true),
        'Y' => (Some(Key::KEY_Y), true),
        'Z' => (Some(Key::KEY_Z), true),

        '0' => (Some(Key::KEY_0), false),
        '1' => (Some(Key::KEY_1), false),
        '2' => (Some(Key::KEY_2), false),
        '3' => (Some(Key::KEY_3), false),
        '4' => (Some(Key::KEY_4), false),
        '5' => (Some(Key::KEY_5), false),
        '6' => (Some(Key::KEY_6), false),
        '7' => (Some(Key::KEY_7), false),
        '8' => (Some(Key::KEY_8), false),
        '9' => (Some(Key::KEY_9), false),

        '!' => (Some(Key::KEY_1), true),
        '@' => (Some(Key::KEY_2), true),
        '#' => (Some(Key::KEY_3), true),
        '$' => (Some(Key::KEY_4), true),
        '%' => (Some(Key::KEY_5), true),
        '^' => (Some(Key::KEY_6), true),
        '&' => (Some(Key::KEY_7), true),
        '*' => (Some(Key::KEY_8), true),
        '(' => (Some(Key::KEY_9), true),
        ')' => (Some(Key::KEY_0), true),

        ' ' => (Some(Key::KEY_SPACE), false),
        // Enter and Tab are keys, not characters, so they're typed by pressing those keys
        '\n' => (Some(Key::KEY_ENTER), false),
        '\t' => (Some(Key::KEY_TAB), false),
        '-' => (Some(Key::KEY_MINUS), false),
        '_' => (Some(Key::KEY_MINUS), true),
        '=' => (Some(Key::KEY_EQUAL), false),
        '+' => (Some(Key::KEY_EQUAL), true),
        '[' => (Some(Key::KEY_LEFTBRACE), false),
        '{' => (Some(Key::KEY_LEFTBRACE), true),
        ']' => (Some(Key::KEY_RIGHTBRACE), false),
        '}' => (Some(Key::KEY_RIGHTBRACE), true),
        '\\' => (Some(Key::KEY_BACKSLASH), false),
        '|' => (Some(Key::KEY_BACKSLASH), true),
        ';' => (Some(Key::KEY_SEMICOLON), false),
        ':' => (Some(Key::KEY_SEMICOLON), true),
        '\'' => (Some(Key::KEY_APOSTROPHE), false),
        '"' => (Some(Key::KEY_APOSTROPHE), true),
        ',' => (Some(Key::KEY_COMMA), false),
        '<' => (Some(Key::KEY_COMMA), true),
        '.' => (Some(Key::KEY_DOT), false),
        '>' => (Some(Key::KEY_DOT), true),
        '/' => (Some(Key::KEY_SLASH), false),
        '?' => (Some(Key::KEY_SLASH), true),
        '`' => (Some(Key::KEY_GRAVE), false),
        '~' => (Some(Key::KEY_GRAVE), true),

        _ => (None, false),
    }
}
