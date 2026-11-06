//! The lookup's stroke-by-stroke animation: how long each stroke of the best result's outline is
//! shown on the keyboard, and which one is up at a given moment.
//!
//! Pure arithmetic on elapsed time, so the Wayland loop only has to ask "which stroke now?" and
//! "how long until that changes?", and the second answer is `None` for an outline of one stroke,
//! which is what keeps the overlay asleep (no timer, no redraws) unless a multi-stroke outline is
//! actually showing.

use std::time::Duration;

/// How long each stroke stays lit before the next one takes its place
pub const STEP: Duration = Duration::from_millis(900);

/// The share of its strength an outline stroke keeps while it is not the one being shown
pub const DIM: f32 = 0.5;

/// Which of `count` strokes is lit after `elapsed`: they take turns in order, looping. A single
/// stroke (or none) is always stroke 0, so it never moves.
#[must_use]
pub fn step_at(count: usize, elapsed: Duration) -> usize {
    if count <= 1 {
        return 0;
    }
    let steps = elapsed.as_millis() / STEP.as_millis();
    usize::try_from(steps % count as u128).unwrap_or(0)
}

/// How long until the lit stroke changes, or `None` when it never does (one stroke or none), so
/// nothing needs to wake up for it.
#[must_use]
pub fn until_next_step(count: usize, elapsed: Duration) -> Option<Duration> {
    if count <= 1 {
        return None;
    }
    let step = STEP.as_millis();
    let left = step - elapsed.as_millis() % step;
    Some(Duration::from_millis(u64::try_from(left).unwrap_or(0)))
}

/// A stroke's alpha while the outline is animating: the stroke being shown keeps all of it, the
/// others are dimmed to [`DIM`].
#[must_use]
pub fn stroke_alpha(alpha: u8, current: bool) -> u8 {
    if current {
        alpha
    } else {
        (f32::from(alpha) * DIM).round() as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn strokes_take_turns_in_order_and_loop() {
        assert_eq!(step_at(2, ms(0)), 0);
        assert_eq!(step_at(2, ms(899)), 0);
        assert_eq!(step_at(2, ms(900)), 1);
        assert_eq!(step_at(2, ms(1799)), 1);
        assert_eq!(step_at(2, ms(1800)), 0, "back to the first");
        assert_eq!(step_at(3, ms(900 * 5)), 2);
        assert_eq!(step_at(3, ms(900 * 6)), 0);
    }

    #[test]
    fn one_stroke_or_none_is_static_and_needs_no_wakeup() {
        for count in [0, 1] {
            assert_eq!(step_at(count, ms(123_456)), 0);
            assert_eq!(until_next_step(count, ms(123_456)), None);
        }
    }

    #[test]
    fn the_wakeup_is_exactly_the_time_left_in_the_current_step() {
        assert_eq!(until_next_step(2, ms(0)), Some(ms(900)));
        assert_eq!(until_next_step(2, ms(250)), Some(ms(650)));
        assert_eq!(until_next_step(2, ms(899)), Some(ms(1)));
        // on a boundary a whole new step starts: wait all of it, never zero (no busy loop)
        assert_eq!(until_next_step(2, ms(900)), Some(ms(900)));
        // waiting that long lands in the next step
        let wait = until_next_step(3, ms(250)).unwrap();
        assert_eq!(step_at(3, ms(250) + wait), 1);
    }

    #[test]
    fn the_stroke_not_being_shown_is_dimmed_to_half_and_the_current_one_is_untouched() {
        assert_eq!(stroke_alpha(255, true), 255);
        assert_eq!(stroke_alpha(255, false), 128);
        assert_eq!(stroke_alpha(200, false), 100);
        assert_eq!(stroke_alpha(0, false), 0);
    }
}
