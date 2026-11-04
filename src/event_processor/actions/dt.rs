//! Tap-dance (`DT`): one action on a tap, another on a second tap within the window, and the
//! first held when the key stays down past the tapping term.
//!
//! DT works with any `KeyAction`, not just a key: when an action fires, it is pressed through
//! its own `emit`, and what that leaves held is let go of when the tap-dance key comes up.
//!
//! Only two of the five outcomes are decided by a key event: the double tap (its second
//! press) and the key being released. The rest are decided by time passing - the term, the
//! window - or by another key going down, and come out of [`DtProcessor::due`] and
//! [`DtProcessor::interrupted_by`] for the keymap to perform.
use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{EmitResult, HeldAction};
use crate::keycode::KeyCode;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Down, and not yet past the term
    Undecided { pressed_at: Instant },
    /// Down past the term: the first action is held until the key comes up
    Holding,
    /// Up after one tap, waiting out the window for a second
    TappedOnce { released_at: Instant },
    /// Down on its second tap: the second action is held until the key comes up
    DoubleTapped,
}

#[derive(Debug, Clone)]
struct Dance {
    key: KeyCode,
    /// The single-tap action, which time alone can decide on
    tap: KeyAction,
    phase: Phase,
}

/// What a tap-dance has been decided to do without a press of its own to do it on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Due {
    /// The key is still down past the term: press `KeyAction` and keep it until the key comes up
    Hold(KeyCode, KeyAction),
    /// The window passed with no second tap: tap `KeyAction`
    Tap(KeyCode, KeyAction),
}

pub struct DtProcessor {
    term: Duration,
    window: Duration,
    /// The keys mid-dance. A handful at most, in the order they started.
    dances: Vec<Dance>,
}

impl DtProcessor {
    pub fn new(config: &Config) -> Self {
        Self {
            term: Duration::from_millis(u64::from(config.tapping_term_ms)),
            window: Duration::from_millis(config.double_tap_window_ms.unwrap_or(250)),
            dances: Vec::new(),
        }
    }

    fn dance_mut(&mut self, key: KeyCode) -> Option<&mut Dance> {
        self.dances.iter_mut().find(|dance| dance.key == key)
    }

    /// The key went down at `at`. `Some` is the second action, to press now: this is the
    /// second tap of the window. `None` is a first press, undecided until the term, the
    /// window, a release or another key decides it.
    ///
    /// Callers settle [`Self::due`] as of `at` first, so a window that passed has already
    /// tapped its action by the time this starts over.
    pub fn on_press(
        &mut self,
        key: KeyCode,
        tap: &KeyAction,
        double_tap: &KeyAction,
        at: Instant,
    ) -> Option<KeyAction> {
        let window = self.window;
        let Some(dance) = self.dance_mut(key) else {
            self.dances.push(Dance {
                key,
                tap: tap.clone(),
                phase: Phase::Undecided { pressed_at: at },
            });
            return None;
        };
        match dance.phase {
            Phase::TappedOnce { released_at }
                if at.saturating_duration_since(released_at) < window =>
            {
                dance.phase = Phase::DoubleTapped;
                Some(double_tap.clone())
            }
            Phase::TappedOnce { .. } => {
                dance.tap = tap.clone();
                dance.phase = Phase::Undecided { pressed_at: at };
                None
            }
            // Pressed while down is a repeat, which changes nothing
            Phase::Undecided { .. } | Phase::Holding | Phase::DoubleTapped => None,
        }
    }

    /// The key came up at `at`. A first press released inside the term is a tap, kept for
    /// the window; anything held is over. What the press pressed is released by the caller.
    pub fn on_release(&mut self, key: KeyCode, at: Instant) {
        let Some(index) = self.dances.iter().position(|dance| dance.key == key) else {
            return;
        };
        match self.dances[index].phase {
            Phase::Undecided { .. } => {
                self.dances[index].phase = Phase::TappedOnce { released_at: at };
            }
            Phase::Holding | Phase::DoubleTapped => {
                self.dances.remove(index);
            }
            Phase::TappedOnce { .. } => {}
        }
    }

    /// What time has decided as of `at`: a first press down past the term is a hold, and a
    /// tap whose window passed with no second tap is a tap.
    pub fn due(&mut self, at: Instant) -> Vec<Due> {
        let (term, window) = (self.term, self.window);
        let mut due = Vec::new();
        self.dances.retain_mut(|dance| match dance.phase {
            Phase::Undecided { pressed_at } if at.saturating_duration_since(pressed_at) >= term => {
                dance.phase = Phase::Holding;
                due.push(Due::Hold(dance.key, dance.tap.clone()));
                true
            }
            Phase::TappedOnce { released_at }
                if at.saturating_duration_since(released_at) >= window =>
            {
                due.push(Due::Tap(dance.key, dance.tap.clone()));
                false
            }
            _ => true,
        });
        due
    }

    /// Another key, `other`, went down: every dance still waiting on time is decided now, so
    /// its action is in place before the other key's. A first press still down is a hold; a
    /// tap waiting for its second is a tap.
    pub fn interrupted_by(&mut self, other: KeyCode) -> Vec<Due> {
        let mut due = Vec::new();
        self.dances.retain_mut(|dance| {
            if dance.key == other {
                return true;
            }
            match dance.phase {
                Phase::Undecided { .. } => {
                    dance.phase = Phase::Holding;
                    due.push(Due::Hold(dance.key, dance.tap.clone()));
                    true
                }
                Phase::TappedOnce { .. } => {
                    due.push(Due::Tap(dance.key, dance.tap.clone()));
                    false
                }
                Phase::Holding | Phase::DoubleTapped => true,
            }
        });
        due
    }

    /// When a dance next needs time checked, if one is waiting on it.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.dances
            .iter()
            .filter_map(|dance| match dance.phase {
                Phase::Undecided { pressed_at } => Some(pressed_at + self.term),
                Phase::TappedOnce { released_at } => Some(released_at + self.window),
                Phase::Holding | Phase::DoubleTapped => None,
            })
            .min()
    }
}

pub fn emit_dt(
    action: &KeyAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    let KeyAction::DT(tap, double_tap) = action else {
        return (EmitResult::None, None);
    };
    let second = ctx.dt_processor.on_press(keycode, tap, double_tap, ctx.at);
    second.map_or(
        (EmitResult::None, Some(HeldAction::DtManaged(None))),
        |second| {
            let (result, held) = second.emit(keycode, ctx);
            let pressed = held.map(|held| Box::new((second, held)));
            (result, Some(HeldAction::DtManaged(pressed)))
        },
    )
}

pub fn unemit_dt(
    action: &KeyAction,
    held_action: HeldAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> EmitResult {
    let (KeyAction::DT(..), HeldAction::DtManaged(pressed)) = (action, held_action) else {
        return EmitResult::None;
    };
    release_dt(pressed, keycode, ctx)
}

/// The tap-dance key came up: whatever its press ended up pressing is let go of.
pub fn release_dt(
    pressed: Option<Box<(KeyAction, HeldAction)>>,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> EmitResult {
    ctx.dt_processor.on_release(keycode, ctx.at);
    pressed.map_or(EmitResult::None, |pressed| {
        let (action, held) = *pressed;
        action.unemit(held, keycode, ctx)
    })
}

/// Every outcome, through the keymap, judged by when the keys moved: `KC_B` is
/// `DT(Key(KC_B), Key(KC_C))` with a 130ms term and a 250ms window, `KC_F1` is
/// `DT(Key(KC_F1), MO("nav"))`, and `KC_D` is a plain key.
#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::event_processor::{KeymapProcessor, ProcessResult};
    use crate::keycode::KeyCode;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    const B: KeyCode = KeyCode::KC_B;
    const C: KeyCode = KeyCode::KC_C;
    const D: KeyCode = KeyCode::KC_D;
    const F1: KeyCode = KeyCode::KC_F1;

    fn processor() -> KeymapProcessor {
        let config: Config = ron::from_str(
            r#"(
                tapping_term_ms: 130,
                double_tap_window_ms: Some(250),
                mt_config: (enable_adaptive_timing: false),
                remaps: {
                    KC_B: DT(Key(KC_B), Key(KC_C)),
                    KC_F1: DT(Key(KC_F1), MO("nav")),
                },
                layers: { "nav": (kind: Remap, remaps: {}) },
            )"#,
        )
        .unwrap();
        KeymapProcessor::new(&config, PathBuf::from("config.ron"), 0)
    }

    /// A moment well before now: events stamped here have waited to be read, and anything
    /// the clock decides has long since been decided.
    fn long_ago() -> Instant {
        Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("the process has been up for two seconds")
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn events(list: &[(KeyCode, bool)]) -> ProcessResult {
        ProcessResult::from_events(list.to_vec())
    }

    #[test]
    fn a_single_tap_is_typed_once_its_window_passes_with_no_second_tap() {
        let mut p = processor();
        let t = Instant::now();
        assert_eq!(p.process_key_at(B, true, t), ProcessResult::None);
        assert_eq!(p.process_key_at(B, false, t + ms(50)), ProcessResult::None);
        // Waiting on the window, which is counted from the release
        assert_eq!(p.next_deadline(), Some(t + ms(50) + ms(250)));
        // Nothing yet: the second tap could still come
        assert_eq!(p.process_key_at(D, false, t + ms(100)), ProcessResult::None);
        // The window passes with the key up: the idle path types the tap
        let mut p = processor();
        let t = long_ago();
        p.process_key_at(B, true, t);
        p.process_key_at(B, false, t + ms(50));
        assert_eq!(p.check_dt_timeouts(), events(&[(B, true), (B, false)]));
        assert_eq!(p.next_deadline(), None);
        // And nothing is typed twice
        assert_eq!(p.check_dt_timeouts(), ProcessResult::None);
    }

    #[test]
    fn a_double_tap_presses_the_second_action_and_releases_it_with_the_key() {
        let mut p = processor();
        let t = long_ago();
        p.process_key_at(B, true, t);
        p.process_key_at(B, false, t + ms(50));
        assert_eq!(
            p.process_key_at(B, true, t + ms(200)),
            ProcessResult::EmitKey(C, true)
        );
        assert_eq!(
            p.process_key_at(B, false, t + ms(260)),
            ProcessResult::EmitKey(C, false)
        );
        // Over: a third press starts a new dance, and time has nothing to add
        assert_eq!(p.check_dt_timeouts(), ProcessResult::None);
        assert_eq!(p.process_key_at(B, true, t + ms(300)), ProcessResult::None);
    }

    #[test]
    fn a_double_tap_on_a_layer_holds_the_layer_until_the_key_comes_up() {
        let mut p = processor();
        let t = long_ago();
        p.process_key_at(F1, true, t);
        p.process_key_at(F1, false, t + ms(50));
        assert_eq!(p.process_key_at(F1, true, t + ms(100)), ProcessResult::None);
        assert_eq!(p.current_layer_name(), "nav");
        assert_eq!(
            p.process_key_at(F1, false, t + ms(400)),
            ProcessResult::None
        );
        assert_eq!(p.current_layer_name(), "base");
    }

    #[test]
    fn held_past_the_term_the_first_action_is_held_until_the_key_comes_up() {
        let mut p = processor();
        let t = long_ago();
        assert_eq!(p.process_key_at(B, true, t), ProcessResult::None);
        assert_eq!(p.next_deadline(), Some(t + ms(130)));
        // The term passes with the key down: the idle path presses the hold
        assert_eq!(p.check_dt_timeouts(), ProcessResult::EmitKey(B, true));
        assert_eq!(p.next_deadline(), None);
        assert_eq!(p.check_dt_timeouts(), ProcessResult::None);
        assert_eq!(
            p.process_key_at(B, false, t + ms(500)),
            ProcessResult::EmitKey(B, false)
        );
    }

    #[test]
    fn a_press_and_release_read_late_in_one_batch_are_judged_by_their_own_times() {
        // Released inside the term: a tap, still waiting for its window
        let mut p = processor();
        let t = long_ago();
        p.process_key_at(B, true, t);
        assert_eq!(p.process_key_at(B, false, t + ms(129)), ProcessResult::None);
        assert_eq!(p.check_dt_timeouts(), events(&[(B, true), (B, false)]));
        // Released past the term: a hold, pressed and released in one go
        let mut p = processor();
        p.process_key_at(B, true, t);
        assert_eq!(
            p.process_key_at(B, false, t + ms(130)),
            events(&[(B, true), (B, false)])
        );
        assert_eq!(p.check_dt_timeouts(), ProcessResult::None);
    }

    #[test]
    fn a_second_press_read_late_outside_the_window_taps_the_first_and_starts_over() {
        let mut p = processor();
        let t = long_ago();
        p.process_key_at(B, true, t);
        p.process_key_at(B, false, t + ms(50));
        // 300ms after the release is past the 250ms window: that tap goes out first
        assert_eq!(
            p.process_key_at(B, true, t + ms(350)),
            events(&[(B, true), (B, false)])
        );
        assert_eq!(p.process_key_at(B, false, t + ms(400)), ProcessResult::None);
        assert_eq!(
            p.process_key_at(B, true, t + ms(500)),
            ProcessResult::EmitKey(C, true)
        );
    }

    #[test]
    fn another_key_while_a_tap_waits_for_its_second_types_the_tap_first() {
        let mut p = processor();
        let t = long_ago();
        p.process_key_at(B, true, t);
        p.process_key_at(B, false, t + ms(50));
        assert_eq!(
            p.process_key_at(D, true, t + ms(100)),
            events(&[(B, true), (B, false), (D, true)])
        );
        assert_eq!(
            p.process_key_at(D, false, t + ms(150)),
            ProcessResult::EmitKey(D, false)
        );
        assert_eq!(p.check_dt_timeouts(), ProcessResult::None);
    }

    #[test]
    fn another_key_while_the_first_press_is_down_holds_the_first_action() {
        let mut p = processor();
        let t = long_ago();
        p.process_key_at(B, true, t);
        assert_eq!(
            p.process_key_at(D, true, t + ms(30)),
            events(&[(B, true), (D, true)])
        );
        assert_eq!(
            p.process_key_at(D, false, t + ms(60)),
            ProcessResult::EmitKey(D, false)
        );
        assert_eq!(
            p.process_key_at(B, false, t + ms(90)),
            ProcessResult::EmitKey(B, false)
        );
        assert_eq!(p.next_deadline(), None);
    }

    #[test]
    fn two_dances_at_once_each_keep_their_own_time() {
        let mut p = processor();
        let t = long_ago();
        p.process_key_at(B, true, t);
        p.process_key_at(B, false, t + ms(40));
        // F1 going down decides B's tap first, then starts its own dance
        assert_eq!(
            p.process_key_at(F1, true, t + ms(60)),
            events(&[(B, true), (B, false)])
        );
        assert_eq!(p.next_deadline(), Some(t + ms(60) + ms(130)));
        assert_eq!(p.check_dt_timeouts(), ProcessResult::EmitKey(F1, true));
        assert_eq!(
            p.process_key_at(F1, false, t + ms(500)),
            ProcessResult::EmitKey(F1, false)
        );
    }
}
