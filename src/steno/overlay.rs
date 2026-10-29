//! What the on-screen overlay shows: the steno layer that is on, the keys in the stroke
//! being written, and the last few strokes of the tape.
//!
//! The key path publishes here, and each `keymux overlay` subscriber reads from here. The
//! hub keeps only the latest state plus a generation counter, not a queue of events. A
//! slow subscriber skips the states it missed, so it can never hold up typing.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// Tape entries kept for the overlay: enough to fill a tall screen, since the tape runs the
/// whole screen height. The overlay shows as many as fit.
pub const TAPE_CAPACITY: usize = 160;

/// One stroke on the on-screen tape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapeEntry {
    /// Increases by one per stroke, so the overlay can tell new lines from ones it has drawn
    pub seq: u64,
    /// Stroke bits in Plover slot order, see [`super::layout::SLOTS`]
    pub stroke: u32,
    /// What the stroke typed, as `keymux steno tape` describes it
    pub text: String,
    /// The stroke matched nothing in the dictionaries
    pub unmatched: bool,
    /// How many times it was repeated by holding it, on top of the once it was typed
    #[serde(default)]
    pub repeats: u32,
}

/// One result of a live lookup: a word, and the outline that types it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupRow {
    pub text: String,
    /// In Plover notation, strokes joined by `/`: `KAPBD` or `KAPB/TKEU`
    pub outline: String,
}

/// What a query means, so the overlay can label the lookup for what it's doing.
///
/// Mirrors [`crate::steno::reverse::Query`] plus the phrase-vs-single-word split
/// [`crate::steno::lookup::Lookup::view`] already makes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LookupMode {
    /// A single word (or nothing yet): the ordinary reverse lookup
    #[default]
    Word,
    /// All-caps steno notation, resolved forward: what would this type?
    Stroke,
    /// A whole phrase (a query with a space in it), reverse-looked-up word by word
    Phrase,
}

/// The live lookup the overlay shows in place of the tape: what is being typed, and what it
/// found. The daemon has already done the dictionary work, so the overlay only draws.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LookupView {
    pub query: String,
    /// Exact matches first, fewest strokes first, then words that start with the query
    pub rows: Vec<LookupRow>,
    /// Stroke bits to show on the keyboard: the first stroke of the best result
    pub hint: u32,
    pub mode: LookupMode,
}

/// Everything the overlay draws. Sent whole on every change: it's a few hundred bytes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlaySnapshot {
    /// Bumped on every change
    pub generation: u64,
    /// The steno layer on top, if any. The overlay is visible while this is `Some`.
    pub layer: Option<String>,
    /// Stroke bits pressed so far in the stroke in progress
    pub chord: u32,
    /// Stroke bits whose keys are down right now, a subset of `chord`
    pub held: u32,
    /// Recent strokes, oldest first
    pub tape: VecDeque<TapeEntry>,
    /// Set while a live lookup is open: shown instead of the tape
    pub lookup: Option<LookupView>,
}

#[derive(Debug, Default)]
pub struct OverlayHub {
    state: Mutex<OverlaySnapshot>,
    changed: Condvar,
}

impl OverlayHub {
    /// The daemon's hub. Global so every keyboard's processor publishes to the same place
    /// without threading a handle through.
    pub fn global() -> &'static Self {
        static HUB: OnceLock<OverlayHub> = OnceLock::new();
        HUB.get_or_init(Self::default)
    }

    /// The steno layer on top changed: `Some` shows the overlay, `None` hides it.
    pub fn set_layer(&self, layer: Option<&str>) {
        self.update(|state| {
            if state.layer.as_deref() == layer {
                return false;
            }
            state.layer = layer.map(str::to_owned);
            // Leaving steno mid-stroke must not leave keys lit for next time, nor a lookup open
            state.chord = 0;
            state.held = 0;
            state.lookup = None;
            true
        });
    }

    /// A live lookup opened, changed or closed (`None`).
    pub fn set_lookup(&self, lookup: Option<LookupView>) {
        self.update(|state| {
            if state.lookup == lookup {
                return false;
            }
            state.lookup = lookup;
            true
        });
    }

    /// A stroke key went down or up.
    pub fn set_keys(&self, chord: u32, held: u32) {
        self.update(|state| {
            let changed = state.chord != chord || state.held != held;
            state.chord = chord;
            state.held = held;
            changed
        });
    }

    /// A stroke finished and was translated. Clears the lit keys.
    pub fn push_stroke(&self, stroke: u32, text: String, unmatched: bool) {
        self.update(|state| {
            // The tape only drops its oldest entry, so the newest one always holds the last seq
            let seq = state.tape.back().map_or(1, |last| last.seq + 1);
            if state.tape.len() == TAPE_CAPACITY {
                state.tape.pop_front();
            }
            state.tape.push_back(TapeEntry {
                seq,
                stroke,
                text,
                unmatched,
                repeats: 0,
            });
            state.chord = 0;
            state.held = 0;
            true
        });
    }

    /// A stroke was typed again by holding it. It counts on its line, if that is the newest one,
    /// instead of adding a line for every repeat.
    pub fn push_repeat(&self, stroke: u32, text: String, unmatched: bool) {
        let counted = self.update(|state| {
            let Some(last) = state.tape.back_mut().filter(|last| last.stroke == stroke) else {
                return false;
            };
            last.repeats += 1;
            state.chord = 0;
            state.held = 0;
            true
        });
        if !counted {
            self.push_stroke(stroke, text, unmatched);
        }
    }

    /// The current state, whatever its generation.
    #[must_use]
    pub fn snapshot(&self) -> OverlaySnapshot {
        lock(&self.state).clone()
    }

    /// Wait until the state differs from generation `seen`, then return it. Returns `None`
    /// after `timeout` with no change, or as soon as `cancel` is set and [`Self::wake`] is
    /// called, so a subscriber thread can stop waiting when its overlay goes away.
    #[must_use]
    pub fn wait_newer(
        &self,
        seen: u64,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Option<OverlaySnapshot> {
        let (state, _) = self
            .changed
            .wait_timeout_while(lock(&self.state), timeout, |state| {
                state.generation == seen && !cancel.load(Ordering::Acquire)
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.generation != seen).then(|| state.clone())
    }

    /// Wake every waiter so it re-checks its `cancel` flag. Taking the lock first means a waiter
    /// that has already checked the flag is parked by the time the notification is sent.
    pub fn wake(&self) {
        drop(lock(&self.state));
        self.changed.notify_all();
    }

    /// Apply `change`, and wake subscribers if it reports that something changed.
    /// Apply `change`, which says whether it changed anything, and wake the subscribers if so.
    fn update(&self, change: impl FnOnce(&mut OverlaySnapshot) -> bool) -> bool {
        let mut state = lock(&self.state);
        let changed = change(&mut state);
        if changed {
            state.generation += 1;
            drop(state);
            self.changed.notify_all();
        }
        changed
    }
}

/// A panic elsewhere must not take the overlay down with it: the state is plain data.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_change_bumps_the_generation_and_a_repeat_does_not() {
        let hub = OverlayHub::default();
        hub.set_layer(Some("steno"));
        hub.set_layer(Some("steno"));
        hub.set_keys(0b11, 0b01);
        hub.set_keys(0b11, 0b01);
        let state = hub.snapshot();
        assert_eq!(state.generation, 2);
        assert_eq!(state.layer.as_deref(), Some("steno"));
        assert_eq!((state.chord, state.held), (0b11, 0b01));
    }

    #[test]
    fn a_finished_stroke_goes_on_the_tape_and_clears_the_keys() {
        let hub = OverlayHub::default();
        hub.set_keys(0b101, 0b100);
        hub.push_stroke(0b101, "\"the\"".into(), false);
        hub.push_stroke(0b1, String::new(), true);
        let state = hub.snapshot();
        assert_eq!((state.chord, state.held), (0, 0));
        let seqs: Vec<u64> = state.tape.iter().map(|entry| entry.seq).collect();
        assert_eq!(seqs, [1, 2]);
        assert!(state.tape[1].unmatched);
    }

    #[test]
    fn a_repeated_stroke_counts_on_its_line_and_wakes_the_overlay() {
        let hub = OverlayHub::default();
        hub.push_stroke(0b1, "\"a\"".into(), false);
        hub.push_stroke(0b10, "\"b\"".into(), false);
        let before = hub.snapshot().generation;
        hub.push_repeat(0b10, "\"b\"".into(), false);
        hub.push_repeat(0b10, "\"b\"".into(), false);
        let state = hub.snapshot();
        assert_eq!(state.tape.len(), 2);
        assert_eq!(state.tape[1].repeats, 2);
        assert_eq!(state.generation, before + 2);
        // A repeat of a stroke that is not the newest line has no line to count on
        hub.push_repeat(0b1, "\"a\"".into(), false);
        assert_eq!(hub.snapshot().tape.len(), 3);
    }

    #[test]
    fn the_tape_keeps_only_the_newest_strokes() {
        let hub = OverlayHub::default();
        for _ in 0..TAPE_CAPACITY + 5 {
            hub.push_stroke(1, String::new(), false);
        }
        let state = hub.snapshot();
        assert_eq!(state.tape.len(), TAPE_CAPACITY);
        assert_eq!(state.tape.front().unwrap().seq, 6);
        assert_eq!(state.tape.back().unwrap().seq, TAPE_CAPACITY as u64 + 5);
    }

    #[test]
    fn a_lookup_is_shown_changed_and_cleared_and_leaving_steno_closes_it() {
        let hub = OverlayHub::default();
        hub.set_layer(Some("steno"));
        let view = |query: &str| LookupView {
            query: query.into(),
            ..LookupView::default()
        };
        hub.set_lookup(Some(view("can")));
        let first = hub.snapshot();
        assert_eq!(first.lookup, Some(view("can")));
        // Setting the same thing again changes nothing, so it doesn't wake the overlay
        hub.set_lookup(Some(view("can")));
        assert_eq!(hub.snapshot().generation, first.generation);
        hub.set_lookup(Some(view("candy")));
        assert!(hub.snapshot().generation > first.generation);

        hub.set_layer(None);
        assert_eq!(hub.snapshot().lookup, None);
    }

    #[test]
    fn leaving_steno_mid_stroke_clears_the_lit_keys() {
        let hub = OverlayHub::default();
        hub.set_layer(Some("steno"));
        hub.set_keys(0b1, 0b1);
        hub.set_layer(None);
        assert_eq!(hub.snapshot().chord, 0);
    }

    #[test]
    fn a_waiting_subscriber_wakes_on_a_change_and_skips_to_the_latest() {
        let hub = Arc::new(OverlayHub::default());
        let waiter = {
            let hub = Arc::clone(&hub);
            std::thread::spawn(move || {
                hub.wait_newer(0, Duration::from_secs(5), &AtomicBool::new(false))
            })
        };
        hub.set_keys(0b1, 0b1);
        hub.set_keys(0b11, 0b11);
        let seen = waiter.join().unwrap().expect("woken by the change");
        assert!(seen.generation >= 1);
        // Asking again from what it saw returns the newest state straight away, if newer
        let latest = hub.snapshot();
        if seen.generation != latest.generation {
            assert_eq!(
                hub.wait_newer(seen.generation, Duration::ZERO, &AtomicBool::new(false)),
                Some(latest)
            );
        }
    }

    #[test]
    fn waiting_with_nothing_new_times_out() {
        let hub = OverlayHub::default();
        let cancel = AtomicBool::new(false);
        assert_eq!(hub.wait_newer(0, Duration::from_millis(10), &cancel), None);
    }

    #[test]
    fn a_cancelled_wait_returns_at_once_without_a_change() {
        let hub = OverlayHub::default();
        let cancel = AtomicBool::new(false);
        std::thread::scope(|scope| {
            // An hour is far longer than the test: only the cancel can end this wait
            let waiter = scope.spawn(|| hub.wait_newer(0, Duration::from_secs(3600), &cancel));
            cancel.store(true, Ordering::Release);
            hub.wake();
            assert_eq!(waiter.join().unwrap(), None);
        });
    }
}
