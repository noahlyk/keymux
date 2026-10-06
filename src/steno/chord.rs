//! Chord tracking: turns individual key presses and releases into strokes.
//!
//! A stroke is every key pressed between the moment the hand leaves the keys and
//! the moment it returns. It completes when the last held key is released, or
//! when `timeout` passes since its first press (a guard against a stuck key).

use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct ChordState {
    /// Stroke bits whose keys are physically down right now.
    held: u32,
    /// Stroke bits accumulated for the stroke in progress.
    stroke: u32,
    /// When the stroke in progress got its first key.
    started: Option<Instant>,
    timeout: Duration,
}

impl ChordState {
    #[must_use]
    pub const fn new(timeout: Duration) -> Self {
        Self {
            held: 0,
            stroke: 0,
            started: None,
            timeout,
        }
    }

    /// A stroke key went down.
    pub const fn press(&mut self, bit: u32, now: Instant) {
        // Bits already held came from another key or an auto-repeat; only new bits count
        let new = bit & !self.held;
        if new == 0 {
            return;
        }
        if self.stroke == 0 {
            self.started = Some(now);
        }
        self.held |= new;
        self.stroke |= new;
    }

    /// A stroke key came up. `bit` is the part of its stroke no other held key still
    /// presses. Returns the finished stroke once nothing is held.
    pub fn release(&mut self, bit: u32) -> Option<u32> {
        self.held &= !bit;
        if self.held == 0 {
            self.take()
        } else {
            None
        }
    }

    /// Finish the stroke if its timeout has passed, even with keys still held.
    pub fn tick(&mut self, now: Instant) -> Option<u32> {
        match self.started {
            Some(start) if now.duration_since(start) >= self.timeout => self.take(),
            _ => None,
        }
    }

    fn take(&mut self) -> Option<u32> {
        let stroke = self.stroke;
        self.stroke = 0;
        self.started = None;
        (stroke != 0).then_some(stroke)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KAT: u32 = (1 << 2) | (1 << 7) | (1 << 18);

    #[test]
    fn stroke_completes_on_last_release() {
        let t0 = Instant::now();
        let mut chord = ChordState::new(Duration::from_secs(1));
        chord.press(1 << 2, t0);
        chord.press(1 << 7, t0);
        chord.press(1 << 18, t0);
        assert_eq!(chord.release(1 << 7), None);
        assert_eq!(chord.release(1 << 2), None);
        assert_eq!(chord.release(1 << 18), Some(KAT));
    }

    #[test]
    fn release_order_does_not_matter() {
        let t0 = Instant::now();
        let mut chord = ChordState::new(Duration::from_secs(1));
        chord.press(1 << 18, t0);
        chord.press(1 << 2, t0);
        chord.press(1 << 7, t0);
        assert_eq!(chord.release(1 << 2), None);
        assert_eq!(chord.release(1 << 18), None);
        assert_eq!(chord.release(1 << 7), Some(KAT));
    }

    #[test]
    fn separate_chords_are_separate_strokes() {
        let t0 = Instant::now();
        let mut chord = ChordState::new(Duration::from_secs(1));
        chord.press(1 << 2, t0);
        assert_eq!(chord.release(1 << 2), Some(1 << 2));
        chord.press(1 << 7, t0);
        assert_eq!(chord.release(1 << 7), Some(1 << 7));
    }

    #[test]
    fn repeated_press_is_ignored() {
        let t0 = Instant::now();
        let mut chord = ChordState::new(Duration::from_secs(1));
        chord.press(1 << 2, t0);
        chord.press(1 << 2, t0);
        assert_eq!(chord.release(1 << 2), Some(1 << 2));
    }

    #[test]
    fn timeout_flushes_a_stuck_stroke() {
        let t0 = Instant::now();
        let mut chord = ChordState::new(Duration::from_millis(500));
        chord.press(1 << 2, t0);
        chord.press(1 << 7, t0);
        assert_eq!(chord.tick(t0 + Duration::from_millis(100)), None);
        assert_eq!(
            chord.tick(t0 + Duration::from_millis(600)),
            Some((1 << 2) | (1 << 7))
        );
        // The stuck keys come up later: nothing new to emit
        assert_eq!(chord.release(1 << 2), None);
        assert_eq!(chord.release(1 << 7), None);
    }
}
