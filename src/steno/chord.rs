//! Chord tracking: turns individual key presses and releases into strokes.
//!
//! A stroke is every key pressed between the moment the hand leaves the keys and
//! the moment it returns. It completes when the last held key is released, however long it
//! was held: no clock ends a stroke (first-up and repeat rules live in the engine).

#[derive(Debug, Default)]
pub struct ChordState {
    /// Stroke bits whose keys are physically down right now.
    held: u32,
    /// Stroke bits accumulated for the stroke in progress.
    stroke: u32,
    /// A key has gone down since the last stroke was sent. A stroke sent early leaves the keys
    /// still held behind, and those alone make no stroke until another key goes down.
    fresh: bool,
}

impl ChordState {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            held: 0,
            stroke: 0,
            fresh: false,
        }
    }

    /// A stroke key went down.
    pub const fn press(&mut self, bit: u32) {
        // Bits already held came from another key or an auto-repeat; only new bits count
        let new = bit & !self.held;
        if new == 0 {
            return;
        }
        self.fresh = true;
        self.held |= new;
        self.stroke |= new;
    }

    /// Forget the chord in progress: no keys held, nothing to send.
    pub const fn clear(&mut self) {
        *self = Self::new();
    }

    /// Whether a key has gone down since the last stroke was sent.
    #[must_use]
    pub const fn is_fresh(&self) -> bool {
        self.fresh
    }

    /// Send the stroke now while keys are still down. The keys still held stay in the chord, so
    /// the next key that goes down makes a stroke with them: hold `#TPH` and tap `R` again and
    /// again. Letting go of the held keys alone sends nothing.
    pub const fn send_early(&mut self) -> Option<u32> {
        if !self.fresh {
            return None;
        }
        let stroke = self.stroke;
        self.stroke = self.held;
        self.fresh = false;
        Some(stroke)
    }

    /// The stroke in progress was already sent some other way (a repeat), so letting go of its
    /// keys must not send it again.
    pub const fn mark_sent(&mut self) {
        self.fresh = false;
    }

    /// Stroke bits pressed so far in the stroke in progress.
    #[must_use]
    pub const fn pending(&self) -> u32 {
        self.stroke
    }

    /// Stroke bits whose keys are down right now.
    #[must_use]
    pub const fn held(&self) -> u32 {
        self.held
    }

    /// A stroke key came up. `bit` is the part of its stroke no other held key still
    /// presses. Returns the finished stroke once nothing is held.
    pub fn release(&mut self, bit: u32) -> Option<u32> {
        self.held &= !bit;
        if !self.fresh {
            // Keys left over from a stroke sent early only count while they are still down
            self.stroke &= !bit;
        }
        if self.held == 0 {
            self.take()
        } else {
            None
        }
    }

    fn take(&mut self) -> Option<u32> {
        let stroke = self.stroke;
        let fresh = self.fresh;
        self.stroke = 0;
        self.fresh = false;
        (fresh && stroke != 0).then_some(stroke)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KAT: u32 = (1 << 2) | (1 << 7) | (1 << 18);

    #[test]
    fn stroke_completes_on_last_release() {
        let mut chord = ChordState::new();
        chord.press(1 << 2);
        chord.press(1 << 7);
        chord.press(1 << 18);
        assert_eq!(chord.release(1 << 7), None);
        assert_eq!(chord.release(1 << 2), None);
        assert_eq!(chord.release(1 << 18), Some(KAT));
    }

    #[test]
    fn release_order_does_not_matter() {
        let mut chord = ChordState::new();
        chord.press(1 << 18);
        chord.press(1 << 2);
        chord.press(1 << 7);
        assert_eq!(chord.release(1 << 2), None);
        assert_eq!(chord.release(1 << 18), None);
        assert_eq!(chord.release(1 << 7), Some(KAT));
    }

    #[test]
    fn separate_chords_are_separate_strokes() {
        let mut chord = ChordState::new();
        chord.press(1 << 2);
        assert_eq!(chord.release(1 << 2), Some(1 << 2));
        chord.press(1 << 7);
        assert_eq!(chord.release(1 << 7), Some(1 << 7));
    }

    #[test]
    fn a_stroke_sent_early_leaves_the_held_keys_to_rearm_with() {
        let mut chord = ChordState::new();
        let (hash, tph, r) = (1 << 22, 1 << 6, 1 << 14);
        chord.press(hash);
        chord.press(tph);
        chord.press(r);
        assert_eq!(chord.release(r), None);
        assert_eq!(chord.send_early(), Some(hash | tph | r));
        // Nothing new went down: no second send, and letting go of the rest sends nothing
        assert_eq!(chord.send_early(), None);
        // Tap R again: the held keys count, so it is the same stroke again
        chord.press(r);
        assert_eq!(chord.release(r), None);
        assert_eq!(chord.send_early(), Some(hash | tph | r));
        assert_eq!(chord.release(tph), None);
        assert_eq!(chord.release(hash), None);
    }

    #[test]
    fn keys_left_after_an_early_send_only_count_while_down() {
        let mut chord = ChordState::new();
        let (a, b, c) = (1 << 2, 1 << 6, 1 << 14);
        chord.press(a);
        chord.press(b);
        chord.release(a);
        assert_eq!(chord.send_early(), Some(a | b));
        chord.press(c);
        assert_eq!(chord.release(c), None);
        assert_eq!(chord.release(b), Some(b | c));
    }

    #[test]
    fn repeated_press_is_ignored() {
        let mut chord = ChordState::new();
        chord.press(1 << 2);
        chord.press(1 << 2);
        assert_eq!(chord.release(1 << 2), Some(1 << 2));
    }

    #[test]
    fn a_stroke_held_a_very_long_time_is_not_sent_until_a_key_comes_up() {
        // The chord keeps no clock: nothing but a key coming up ends a stroke
        let mut chord = ChordState::new();
        chord.press(1 << 2);
        chord.press(1 << 7);
        assert_eq!(chord.pending(), (1 << 2) | (1 << 7));
        assert_eq!(chord.release(1 << 2), None);
        assert_eq!(chord.release(1 << 7), Some((1 << 2) | (1 << 7)));
    }
}
