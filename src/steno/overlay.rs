//! What the on-screen overlay shows: the steno layer that is on, the keys in the stroke
//! being written, and the last few strokes of the tape.
//!
//! The key path publishes here, and each `keymux overlay` subscriber reads from here. The
//! hub keeps only the latest state plus a generation counter, not a queue of events. A
//! slow subscriber skips the states it missed, so it can never hold up typing.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

/// Tape entries kept for the overlay. More than any panel shows; the overlay picks its own count.
pub const TAPE_CAPACITY: usize = 32;

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
            // Leaving steno mid-stroke must not leave keys lit for next time
            state.chord = 0;
            state.held = 0;
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
            });
            state.chord = 0;
            state.held = 0;
            true
        });
    }

    /// The current state, whatever its generation.
    #[must_use]
    pub fn snapshot(&self) -> OverlaySnapshot {
        lock(&self.state).clone()
    }

    /// Wait until the state differs from generation `seen`, then return it. Returns `None`
    /// after `timeout` with no change, so a subscriber thread can check its socket.
    #[must_use]
    pub fn wait_newer(&self, seen: u64, timeout: Duration) -> Option<OverlaySnapshot> {
        let state = lock(&self.state);
        let (state, _) = self
            .changed
            .wait_timeout_while(state, timeout, |state| state.generation == seen)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.generation != seen).then(|| state.clone())
    }

    /// Apply `change`, and wake subscribers if it reports that something changed.
    fn update(&self, change: impl FnOnce(&mut OverlaySnapshot) -> bool) {
        let mut state = lock(&self.state);
        if change(&mut state) {
            state.generation += 1;
            drop(state);
            self.changed.notify_all();
        }
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
            std::thread::spawn(move || hub.wait_newer(0, Duration::from_secs(5)))
        };
        hub.set_keys(0b1, 0b1);
        hub.set_keys(0b11, 0b11);
        let seen = waiter.join().unwrap().expect("woken by the change");
        assert!(seen.generation >= 1);
        // Asking again from what it saw returns the newest state straight away, if newer
        let latest = hub.snapshot();
        if seen.generation != latest.generation {
            assert_eq!(
                hub.wait_newer(seen.generation, Duration::ZERO),
                Some(latest)
            );
        }
    }

    #[test]
    fn waiting_with_nothing_new_times_out() {
        let hub = OverlayHub::default();
        assert_eq!(hub.wait_newer(0, Duration::from_millis(10)), None);
    }
}
