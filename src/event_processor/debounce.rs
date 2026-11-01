//! Switch-bounce protection: a single physical key actuation occasionally reaches us as two
//! separate press/release pairs a few milliseconds apart (worn or dirty switch contacts), not
//! because of anything the user did. Left unfiltered, that phantom second press reads as a real
//! second stroke downstream — on a steno layer this can retype or re-undo something the user
//! only did once, which is far more disruptive than a doubled letter on a plain layer would be.
//!
//! This only ever looks at one physical key's own press/release timing against itself; it has no
//! opinion about chords, strokes, or repeats, and sits in front of all of that (raw evdev codes,
//! before `KeyCode::from_evdev_code` even runs) so every layer benefits, not just steno.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

/// Below this, a press landing right after that same key's own last release is contact chatter,
/// not a second, intentional actuation — even the fastest deliberate double-tap of one key (the
/// same finger, lifting and pressing again) takes noticeably longer than this to a human, while
/// bounce on a worn switch typically resolves within single-digit milliseconds.
const WINDOW: Duration = Duration::from_millis(25);

/// Tracks, per physical keyboard device, the last release time of every raw key code seen, and
/// which codes currently have a chatter press still "open" (so its own matching release gets
/// dropped too, instead of reaching the keymap as a release with no press behind it).
#[derive(Default)]
pub struct Debounce {
    last_release: HashMap<u16, Instant>,
    dropped_press: HashSet<u16>,
}

impl Debounce {
    /// Whether this press or release of `code` should be dropped as chatter.
    pub fn is_chatter(&mut self, code: u16, pressed: bool, now: Instant) -> bool {
        if pressed {
            let chatter = self
                .last_release
                .get(&code)
                .is_some_and(|&released_at| now.saturating_duration_since(released_at) < WINDOW);
            if chatter {
                self.dropped_press.insert(code);
            }
            chatter
        } else if self.dropped_press.remove(&code) {
            // This release's own press was already dropped - drop the release too, so the
            // keymap never sees a lone release with no press behind it.
            true
        } else {
            self.last_release.insert(code, now);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_right_after_that_keys_own_release_is_chatter() {
        let mut debounce = Debounce::default();
        let t0 = Instant::now();
        assert!(!debounce.is_chatter(30, false, t0), "a release is never chatter on its own");
        assert!(
            debounce.is_chatter(30, true, t0 + Duration::from_millis(5)),
            "5ms after that same key's release is well inside bounce territory"
        );
    }

    #[test]
    fn the_release_matching_a_dropped_chatter_press_is_dropped_too() {
        let mut debounce = Debounce::default();
        let t0 = Instant::now();
        debounce.is_chatter(30, false, t0);
        assert!(debounce.is_chatter(30, true, t0 + Duration::from_millis(5)));
        assert!(
            debounce.is_chatter(30, false, t0 + Duration::from_millis(10)),
            "the release for the press we just dropped must be dropped too, not sent on its own"
        );
    }

    #[test]
    fn a_press_well_after_that_keys_own_release_is_not_chatter() {
        let mut debounce = Debounce::default();
        let t0 = Instant::now();
        debounce.is_chatter(30, false, t0);
        assert!(
            !debounce.is_chatter(30, true, t0 + Duration::from_millis(100)),
            "100ms is a real, deliberate second press of the same key"
        );
        assert!(
            !debounce.is_chatter(30, false, t0 + Duration::from_millis(150)),
            "a release matching a genuine press must never be dropped"
        );
    }

    #[test]
    fn two_different_keys_never_debounce_each_other() {
        let mut debounce = Debounce::default();
        let t0 = Instant::now();
        debounce.is_chatter(30, false, t0);
        assert!(
            !debounce.is_chatter(31, true, t0 + Duration::from_millis(1)),
            "a different key's press must never be judged against another key's release"
        );
    }

    #[test]
    fn a_keys_very_first_press_is_never_chatter() {
        let mut debounce = Debounce::default();
        assert!(
            !debounce.is_chatter(30, true, Instant::now()),
            "with no prior release recorded for this key, there is nothing to debounce against"
        );
    }

    #[test]
    fn chatter_is_judged_against_the_most_recent_release_not_a_stale_one() {
        let mut debounce = Debounce::default();
        let t0 = Instant::now();
        debounce.is_chatter(30, false, t0);
        // A real press-hold-release cycle happens in between, well clear of the window.
        assert!(!debounce.is_chatter(30, true, t0 + Duration::from_millis(200)));
        debounce.is_chatter(30, false, t0 + Duration::from_millis(250));
        // Now a second release just happened - a press 5ms after *that* is chatter, even though
        // it is also comfortably far from the first, much older release.
        assert!(debounce.is_chatter(30, true, t0 + Duration::from_millis(255)));
    }
}
