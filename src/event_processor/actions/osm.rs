//! One-shot modifiers (`OSM`), after QMK.
//!
//! The modifier goes down with the key, as any modifier would. Tapped - up within the tapping
//! term with nothing pressed under it - it stays down, armed for the next key: that key is
//! pressed and released under it, and then it comes up. Held past the term, or with a key
//! pressed under it, it is an ordinary modifier until the key comes up. Armed and left alone
//! past `oneshot_timeout_ms`, it comes up by itself, from the idle path when no key prompts it.
//! Tapped again while armed, it is re-armed with the timeout started over; nothing is pressed
//! twice.
use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{EmitResult, HeldAction};
use crate::keycode::KeyCode;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// An OSM key that is physically down.
#[derive(Debug, Clone, Copy)]
struct Held {
    modifier: KeyCode,
    pressed_at: Instant,
    /// A key was pressed under it: a hold, however short
    used: bool,
}

/// A modifier a tap left down, waiting for the key it applies to.
#[derive(Debug, Clone, Copy)]
struct Armed {
    modifier: KeyCode,
    since: Instant,
    /// The key pressed under it, once one has been: the modifier comes up when that key does
    consumer: Option<KeyCode>,
}

pub struct OsmProcessor {
    timeout: Duration,
    tapping_term: Duration,
    /// By the physical key
    held: HashMap<KeyCode, Held>,
    armed: Vec<Armed>,
}

impl OsmProcessor {
    pub fn new(config: &Config) -> Self {
        Self {
            timeout: Duration::from_millis(config.oneshot_timeout_ms.unwrap_or(5000)),
            tapping_term: Duration::from_millis(u64::from(config.tapping_term_ms)),
            held: HashMap::new(),
            armed: Vec::new(),
        }
    }

    fn is_held(&self, modifier: KeyCode) -> bool {
        self.held.values().any(|held| held.modifier == modifier)
    }

    fn is_down(&self, modifier: KeyCode) -> bool {
        self.is_held(modifier) || self.armed.iter().any(|armed| armed.modifier == modifier)
    }

    /// The OSM key `key` for `modifier` went down at `at`. True when the modifier is to be
    /// pressed now; false when it is down already, armed or held by another key.
    #[must_use]
    pub fn on_press(&mut self, key: KeyCode, modifier: KeyCode, at: Instant) -> bool {
        let was_down = self.is_down(modifier);
        // Back in hand: no longer armed, the key decides again when it comes up
        self.armed.retain(|armed| armed.modifier != modifier);
        self.held.insert(
            key,
            Held {
                modifier,
                pressed_at: at,
                used: false,
            },
        );
        !was_down
    }

    /// The OSM key `key` came up at `at`. A tap arms the modifier, which stays down; a hold, or
    /// a key used under it, lets it go: `Some(modifier)` is to be released now.
    #[must_use]
    pub fn on_release(&mut self, key: KeyCode, at: Instant) -> Option<KeyCode> {
        let held = self.held.remove(&key)?;
        let tapped =
            !held.used && at.saturating_duration_since(held.pressed_at) < self.tapping_term;
        self.armed.retain(|armed| armed.modifier != held.modifier);
        if tapped {
            self.armed.push(Armed {
                modifier: held.modifier,
                since: at,
                consumer: None,
            });
            return None;
        }
        // Another OSM key may still be holding the same modifier down
        (!self.is_held(held.modifier)).then_some(held.modifier)
    }

    /// Another key went down: every armed modifier applies to it, and every OSM key still
    /// down is a hold from now on.
    pub fn on_other_key_press(&mut self, key: KeyCode) {
        for held in self.held.values_mut() {
            held.used = true;
        }
        for armed in &mut self.armed {
            if armed.consumer.is_none() {
                armed.consumer = Some(key);
            }
        }
    }

    /// The key `key` came up: the modifiers it was pressed under come up with it.
    #[must_use]
    pub fn on_other_key_release(&mut self, key: KeyCode) -> Vec<KeyCode> {
        self.take(|armed| armed.consumer == Some(key))
    }

    /// Armed modifiers no key has used within the timeout, as of `at`: they come up.
    #[must_use]
    pub fn expire(&mut self, at: Instant) -> Vec<KeyCode> {
        let timeout = self.timeout;
        self.take(|armed| {
            armed.consumer.is_none() && at.saturating_duration_since(armed.since) >= timeout
        })
    }

    /// Every armed modifier comes up, used or not: the layer under it has changed.
    #[must_use]
    pub fn release_all(&mut self) -> Vec<KeyCode> {
        self.take(|_| true)
    }

    /// When the next armed modifier times out, if one is waiting.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        self.armed
            .iter()
            .filter(|armed| armed.consumer.is_none())
            .map(|armed| armed.since + self.timeout)
            .min()
    }

    /// Drop the armed modifiers `done` picks out, and say which are to be released: those no
    /// OSM key is still holding down.
    fn take(&mut self, done: impl Fn(&Armed) -> bool) -> Vec<KeyCode> {
        let (taken, kept): (Vec<_>, Vec<_>) = self.armed.drain(..).partition(done);
        self.armed = kept;
        taken
            .into_iter()
            .map(|armed| armed.modifier)
            .filter(|&modifier| !self.is_held(modifier))
            .collect()
    }
}

/// Whether pressing `action` on `keycode` is what an armed one-shot waits for: a key to go
/// under it, rather than another modifier, a layer switch or a command.
#[must_use]
pub const fn consumes_one_shot(action: &KeyAction, keycode: KeyCode) -> bool {
    match action {
        KeyAction::Key(key) => !key.is_modifier(),
        KeyAction::Transparent => !keycode.is_modifier(),
        KeyAction::MT(..) | KeyAction::DT(..) | KeyAction::SOCD(..) | KeyAction::MULTI(..) => true,
        KeyAction::OSM(..)
        | KeyAction::TO(..)
        | KeyAction::TG(..)
        | KeyAction::MO(..)
        | KeyAction::CMD(..) => false,
    }
}

pub fn emit_osm(
    action: &KeyAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    let KeyAction::OSM(modifier_action) = action else {
        return (EmitResult::None, None);
    };
    let Some(modifier) = modifier_action.as_keycode() else {
        return (EmitResult::None, None);
    };
    let result = if ctx.osm_processor.on_press(keycode, modifier, ctx.at) {
        EmitResult::EmitKey(modifier, true)
    } else {
        EmitResult::None
    };
    (result, Some(HeldAction::OsmManaged))
}

pub fn unemit_osm(
    action: &KeyAction,
    held_action: HeldAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> EmitResult {
    match (action, held_action) {
        (KeyAction::OSM(_), HeldAction::OsmManaged) => ctx
            .osm_processor
            .on_release(keycode, ctx.at)
            .map_or(EmitResult::None, |modifier| {
                EmitResult::EmitKey(modifier, false)
            }),
        _ => EmitResult::None,
    }
}

/// Every event is judged by when the keyboard reported it, so these stamp their events a second
/// in the past: what the keyboard saw as a tap is a tap however late it was read.
#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::event_processor::{KeymapProcessor, ProcessResult};
    use crate::keycode::KeyCode;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const OSM: KeyCode = KeyCode::KC_CAPS;
    const SHIFT: KeyCode = KeyCode::KC_LSFT;
    const A: KeyCode = KeyCode::KC_A;
    const B: KeyCode = KeyCode::KC_B;
    const TIMEOUT: Duration = Duration::from_millis(500);

    fn processor() -> KeymapProcessor {
        let config: Config = ron::from_str(
            r#"(
                tapping_term_ms: 130,
                oneshot_timeout_ms: Some(500),
                remaps: {
                    KC_CAPS: OSM(Key(KC_LSFT)),
                    KC_F1: TO("nav"),
                },
                layers: { "nav": (kind: Remap, remaps: {}) },
            )"#,
        )
        .unwrap();
        KeymapProcessor::new(&config, PathBuf::from("config.ron"), 0)
    }

    /// A moment well before now: events stamped here have waited to be read.
    fn long_ago() -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("the process has been up for a second")
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn press(p: &mut KeymapProcessor, key: KeyCode, at: Instant) -> ProcessResult {
        p.process_key_at(key, true, at)
    }

    fn release(p: &mut KeymapProcessor, key: KeyCode, at: Instant) -> ProcessResult {
        p.process_key_at(key, false, at)
    }

    fn down(key: KeyCode) -> ProcessResult {
        ProcessResult::EmitKey(key, true)
    }

    fn up(key: KeyCode) -> ProcessResult {
        ProcessResult::EmitKey(key, false)
    }

    /// Tap the OSM key at `t`: the modifier goes down and stays down.
    fn tap(p: &mut KeymapProcessor, t: Instant) {
        assert_eq!(press(p, OSM, t), down(SHIFT));
        assert_eq!(release(p, OSM, t + ms(50)), ProcessResult::None);
    }

    #[test]
    fn a_tap_holds_the_modifier_around_the_next_key_and_no_further() {
        let mut p = processor();
        let t = long_ago();
        tap(&mut p, t);
        assert_eq!(press(&mut p, A, t + ms(200)), down(A));
        assert_eq!(
            release(&mut p, A, t + ms(250)),
            ProcessResult::MultipleEvents(vec![(A, false), (SHIFT, false)])
        );
        assert_eq!(press(&mut p, B, t + ms(300)), down(B));
        assert_eq!(release(&mut p, B, t + ms(350)), up(B));
    }

    #[test]
    fn a_tap_read_late_is_still_a_tap_and_the_key_read_with_it_is_under_it() {
        let mut p = processor();
        // A second old, well past the timeout by the clock on the wall, but by the keyboard's
        // clock A came 100ms after the tap
        let t = long_ago();
        tap(&mut p, t);
        assert_eq!(press(&mut p, A, t + ms(100)), down(A));
        assert_eq!(
            release(&mut p, A, t + ms(150)),
            ProcessResult::MultipleEvents(vec![(A, false), (SHIFT, false)])
        );
    }

    #[test]
    fn every_key_pressed_before_the_first_comes_up_is_under_it() {
        let mut p = processor();
        let t = long_ago();
        tap(&mut p, t);
        assert_eq!(press(&mut p, A, t + ms(200)), down(A));
        assert_eq!(press(&mut p, B, t + ms(220)), down(B));
        assert_eq!(
            release(&mut p, A, t + ms(260)),
            ProcessResult::MultipleEvents(vec![(A, false), (SHIFT, false)])
        );
        assert_eq!(release(&mut p, B, t + ms(280)), up(B));
    }

    #[test]
    fn held_past_the_term_it_is_an_ordinary_modifier() {
        let mut p = processor();
        let t = long_ago();
        assert_eq!(press(&mut p, OSM, t), down(SHIFT));
        assert_eq!(release(&mut p, OSM, t + ms(300)), up(SHIFT));
        assert_eq!(press(&mut p, A, t + ms(400)), down(A));
        assert_eq!(release(&mut p, A, t + ms(450)), up(A));
    }

    #[test]
    fn a_key_pressed_under_it_makes_it_a_hold_however_short() {
        let mut p = processor();
        let t = long_ago();
        assert_eq!(press(&mut p, OSM, t), down(SHIFT));
        assert_eq!(press(&mut p, A, t + ms(20)), down(A));
        assert_eq!(release(&mut p, A, t + ms(40)), up(A));
        assert_eq!(release(&mut p, OSM, t + ms(60)), up(SHIFT));
        assert_eq!(p.next_deadline(), None);
    }

    #[test]
    fn left_alone_past_the_timeout_it_comes_up_from_the_idle_path() {
        let mut p = processor();
        let t = long_ago();
        tap(&mut p, t);
        assert_eq!(p.next_deadline(), Some(t + ms(50) + TIMEOUT));
        assert_eq!(p.check_osm_timeouts(), up(SHIFT));
        assert_eq!(p.next_deadline(), None);
        assert_eq!(p.check_osm_timeouts(), ProcessResult::None);
        assert_eq!(press(&mut p, A, Instant::now()), down(A));
        assert_eq!(release(&mut p, A, Instant::now()), up(A));
    }

    #[test]
    fn a_key_read_after_the_timeout_sees_it_come_up_first() {
        let mut p = processor();
        let t = long_ago();
        tap(&mut p, t);
        assert_eq!(
            press(&mut p, A, t + ms(600)),
            ProcessResult::MultipleEvents(vec![(SHIFT, false), (A, true)])
        );
        assert_eq!(release(&mut p, A, t + ms(650)), up(A));
    }

    #[test]
    fn tapped_again_while_armed_it_is_re_armed_without_a_second_press() {
        let mut p = processor();
        let t = long_ago();
        tap(&mut p, t);
        assert_eq!(press(&mut p, OSM, t + ms(400)), ProcessResult::None);
        assert_eq!(release(&mut p, OSM, t + ms(450)), ProcessResult::None);
        assert_eq!(p.next_deadline(), Some(t + ms(450) + TIMEOUT));
        // Past the first arming's timeout, inside the second's
        assert_eq!(press(&mut p, A, t + ms(800)), down(A));
        assert_eq!(
            release(&mut p, A, t + ms(850)),
            ProcessResult::MultipleEvents(vec![(A, false), (SHIFT, false)])
        );
    }

    #[test]
    fn held_again_while_armed_it_is_a_plain_hold_let_go_once() {
        let mut p = processor();
        let t = long_ago();
        tap(&mut p, t);
        assert_eq!(press(&mut p, OSM, t + ms(100)), ProcessResult::None);
        assert_eq!(release(&mut p, OSM, t + ms(400)), up(SHIFT));
        assert_eq!(p.next_deadline(), None);
        assert_eq!(press(&mut p, A, t + ms(500)), down(A));
        assert_eq!(release(&mut p, A, t + ms(550)), up(A));
    }

    #[test]
    fn another_modifier_does_not_use_it_up() {
        let mut p = processor();
        let t = long_ago();
        tap(&mut p, t);
        assert_eq!(
            press(&mut p, KeyCode::KC_LCTL, t + ms(100)),
            down(KeyCode::KC_LCTL)
        );
        assert_eq!(press(&mut p, A, t + ms(150)), down(A));
        assert_eq!(
            release(&mut p, A, t + ms(200)),
            ProcessResult::MultipleEvents(vec![(A, false), (SHIFT, false)])
        );
        assert_eq!(
            release(&mut p, KeyCode::KC_LCTL, t + ms(250)),
            up(KeyCode::KC_LCTL)
        );
    }

    #[test]
    fn a_layer_switch_lets_it_go() {
        let mut p = processor();
        let t = long_ago();
        tap(&mut p, t);
        assert_eq!(press(&mut p, KeyCode::KC_F1, t + ms(100)), up(SHIFT));
        assert_eq!(p.current_layer_name(), "nav");
        assert_eq!(
            release(&mut p, KeyCode::KC_F1, t + ms(150)),
            ProcessResult::None
        );
        assert_eq!(p.next_deadline(), None);
        assert_eq!(press(&mut p, A, t + ms(200)), down(A));
        assert_eq!(release(&mut p, A, t + ms(250)), up(A));
    }
}
