//! Split-keyboard skew compensation and measurement.
//!
//! When one half of a split keyboard reaches the host later than the other (a wireless half), a
//! chord pressed with both hands can finish on the fast half before the slow half's first key
//! arrives, and one stroke becomes two.
//!
//! Two parts. [`Delay`] holds the fast half's key events back by a fixed time and replays them in
//! order (see `StenoConfig::skew`). [`Tracker`] watches the raw arrival times of stroke keys and
//! feeds [`Stats`], which `keymux steno skew` reads from the daemon to suggest a delay and show
//! what it would cost. Only counts and millisecond differences are kept, never which keys.

use super::layout::Layout;
use crate::config::{SkewConfig, SkewSide, MAX_SKEW_DELAY_MS};
use crate::keycode::KeyCode;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Which part of the layout a stroke key is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Left,
    Right,
    /// The star, the number key, extra keys, or a key that presses keys of both hands.
    Center,
}

impl Class {
    const fn bit(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Center => 4,
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Center => 2,
        }
    }
}

/// The half of every stroke key, from the layout's hand boundaries.
#[must_use]
pub fn classify(layout: &Layout, keys: &HashMap<KeyCode, u32>) -> HashMap<KeyCode, Class> {
    let (left, right) = layout.hand_masks();
    keys.iter()
        .map(|(&key, &bits)| {
            let class = if bits != 0 && bits & !left == 0 {
                Class::Left
            } else if bits != 0 && bits & !right == 0 {
                Class::Right
            } else {
                Class::Center
            };
            (key, class)
        })
        .collect()
}

/// The physical keys a config delays: its `half` plus its `keys`.
pub fn delayed_keys(
    config: &SkewConfig,
    layout: &Layout,
    keys: &HashMap<KeyCode, u32>,
) -> Result<HashSet<KeyCode>, String> {
    let classes = classify(layout, keys);
    let mut set = HashSet::new();
    let wanted = match config.half {
        SkewSide::None => None,
        SkewSide::Left => Some(Class::Left),
        SkewSide::Right => Some(Class::Right),
    };
    if let Some(wanted) = wanted {
        set.extend(
            classes
                .iter()
                .filter(|(_, &c)| c == wanted)
                .map(|(&k, _)| k),
        );
    }
    for key in &config.keys {
        if !keys.contains_key(key) {
            return Err(format!(
                "skew key {key:?} is not a stroke key on this layer"
            ));
        }
        set.insert(*key);
    }
    Ok(set)
}

/// What `keymux validate` says about a layer's `skew`: `Err` is an error.
pub fn check_config(
    config: &SkewConfig,
    layout: &Layout,
    keys: &HashMap<KeyCode, u32>,
) -> Result<(), String> {
    if config.delay_ms > MAX_SKEW_DELAY_MS {
        return Err(format!(
            "skew delay_ms {} is above the {MAX_SKEW_DELAY_MS} ms maximum",
            config.delay_ms
        ));
    }
    let set = delayed_keys(config, layout, keys)?;
    if config.delay_ms > 0 && set.is_empty() {
        return Err("skew delay_ms is set but `half` and `keys` select no stroke keys".to_string());
    }
    Ok(())
}

/// One held-back key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Queued {
    pub key: KeyCode,
    pub pressed: bool,
    /// When it is replayed, and the moment it is replayed as: arrival plus the delay.
    pub due: Instant,
}

/// The fast half's events, held back by a fixed delay and replayed in order.
#[derive(Debug, Default)]
pub struct Delay {
    wait: Duration,
    keys: HashSet<KeyCode>,
    queue: VecDeque<Queued>,
}

impl Delay {
    #[must_use]
    pub fn new(delay_ms: u32, keys: HashSet<KeyCode>) -> Self {
        let wait = Duration::from_millis(u64::from(delay_ms.min(MAX_SKEW_DELAY_MS)));
        Self {
            wait,
            keys,
            queue: VecDeque::new(),
        }
    }

    /// Whether any key is delayed at all. When not, nothing is ever queued.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.wait.is_zero() && !self.keys.is_empty()
    }

    #[must_use]
    pub const fn delay(&self) -> Duration {
        self.wait
    }

    #[must_use]
    pub fn delays(&self, key: KeyCode) -> bool {
        self.keys.contains(&key)
    }

    pub fn push(&mut self, key: KeyCode, pressed: bool, at: Instant) {
        self.queue.push_back(Queued {
            key,
            pressed,
            due: at + self.wait,
        });
    }

    /// The oldest event that is due at `now`.
    pub fn pop_due(&mut self, now: Instant) -> Option<Queued> {
        if self.queue.front()?.due <= now {
            self.queue.pop_front()
        } else {
            None
        }
    }

    /// When the oldest queued event is due.
    #[must_use]
    pub fn next_due(&self) -> Option<Instant> {
        self.queue.front().map(|queued| queued.due)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Make everything queued due now, in the same order (the layer was left).
    pub fn expedite(&mut self, now: Instant) {
        for queued in &mut self.queue {
            queued.due = queued.due.min(now);
        }
    }
}

/// Widest first-down difference kept, in ms either way.
pub const DIFF_RANGE: i64 = 60;
/// Two strokes of opposite halves starting this close are a split suspect.
pub const SPLIT_WINDOW: Duration = Duration::from_millis(40);
/// How many recent strokes the added-latency figures cover.
pub const RECENT: usize = 500;
/// Two-hand strokes needed before a suggestion is made.
pub const ENOUGH: u32 = 200;
/// Longest gap, in ms, that a delay can still change.
const GAP_MAX: usize = MAX_SKEW_DELAY_MS as usize;

/// What one finished stroke contributes, measured on arrival times.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Obs {
    /// Right's first key down minus left's, in ms, for a stroke with keys of both halves.
    pub diff_ms: Option<i64>,
    /// Per half (left, right): how long after that half's last key up the other keys' last key
    /// up came, in ms. `None` when the half is not in the stroke or nothing else is.
    pub gap_ms: [Option<Option<u32>>; 2],
    /// Split suspect: the half of the first stroke and the delay that would have joined them.
    pub split: Option<(usize, u32)>,
    /// The latency the configured delay added to this stroke, in ms.
    pub added_ms: u32,
}

fn micros(from: Instant, to: Instant) -> i64 {
    let signed = |d: Duration| i64::try_from(d.as_micros()).unwrap_or(i64::MAX);
    if to >= from {
        signed(to - from)
    } else {
        -signed(from - to)
    }
}

/// Reads stroke keys' raw arrival times, strokes as the keys' last release ends them.
#[derive(Debug, Default)]
pub struct Tracker {
    classes: HashMap<KeyCode, Class>,
    delayed: HashSet<KeyCode>,
    delay: Duration,
    down: HashSet<KeyCode>,
    cur: Option<Cur>,
    prev: Option<Prev>,
}

#[derive(Debug, Clone, Copy)]
struct Cur {
    start: Instant,
    mask: u8,
    first: [Option<Instant>; 2],
    last_up: [Option<Instant>; 3],
    last_delayed: Option<Instant>,
    last_other: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct Prev {
    start: Instant,
    mask: u8,
    last_up: Instant,
}

impl Tracker {
    #[must_use]
    pub fn new(
        classes: HashMap<KeyCode, Class>,
        delayed: HashSet<KeyCode>,
        delay: Duration,
    ) -> Self {
        Self {
            classes,
            delayed,
            delay,
            ..Self::default()
        }
    }

    /// Whether the layout has two hands to compare.
    #[must_use]
    pub fn is_useful(&self) -> bool {
        let has = |wanted| self.classes.values().any(|&c| c == wanted);
        has(Class::Left) && has(Class::Right)
    }

    /// A stroke key went down or up at `at` (as the keyboard reported it, before any delay).
    /// Returns the finished stroke's measurements when this was its last key up.
    pub fn event(&mut self, key: KeyCode, pressed: bool, at: Instant) -> Option<Obs> {
        let class = *self.classes.get(&key)?;
        if pressed {
            if !self.down.insert(key) {
                return None;
            }
            let cur = self.cur.get_or_insert(Cur {
                start: at,
                mask: 0,
                first: [None; 2],
                last_up: [None; 3],
                last_delayed: None,
                last_other: None,
            });
            cur.mask |= class.bit();
            if class != Class::Center && cur.first[class.index()].is_none() {
                cur.first[class.index()] = Some(at);
            }
            return None;
        }
        if !self.down.remove(&key) {
            return None;
        }
        let cur = self.cur.as_mut()?;
        cur.last_up[class.index()] = Some(at);
        let slot = if self.delayed.contains(&key) {
            &mut cur.last_delayed
        } else {
            &mut cur.last_other
        };
        *slot = Some(slot.map_or(at, |old| old.max(at)));
        if !self.down.is_empty() {
            return None;
        }
        let cur = self.cur.take()?;
        Some(self.finish(&cur))
    }

    fn finish(&mut self, cur: &Cur) -> Obs {
        let mut obs = Obs::default();
        if let [Some(left), Some(right)] = cur.first {
            obs.diff_ms = Some((micros(left, right) + 500).div_euclid(1000));
        }
        for side in 0..2 {
            let Some(own) = cur.last_up[side] else {
                continue;
            };
            let other = [0, 1, 2]
                .into_iter()
                .filter(|&i| i != side)
                .filter_map(|i| cur.last_up[i])
                .max();
            obs.gap_ms[side] = Some(
                other.and_then(|other| u32::try_from(micros(own, other)).ok().map(|us| us / 1000)),
            );
        }
        obs.added_ms = match (cur.last_delayed, cur.last_other) {
            (Some(delayed), Some(other)) => {
                let shifted = delayed + self.delay;
                let after = shifted.max(other);
                let before = delayed.max(other);
                u32::try_from((micros(before, after) + 500) / 1000).unwrap_or(0)
            }
            (Some(_), None) => u32::try_from(self.delay.as_millis()).unwrap_or(0),
            _ => 0,
        };
        let last_up = cur
            .last_up
            .iter()
            .flatten()
            .max()
            .copied()
            .unwrap_or(cur.start);
        if let Some(prev) = self.prev {
            let opposite = (prev.mask == 1 && cur.mask == 2) || (prev.mask == 2 && cur.mask == 1);
            if opposite && cur.start.saturating_duration_since(prev.start) <= SPLIT_WINDOW {
                let gap_us = micros(prev.last_up, cur.start).max(0);
                let needed = u32::try_from(gap_us / 1000 + 1).unwrap_or(u32::MAX);
                obs.split = Some((usize::from(prev.mask == 2), needed));
            }
        }
        self.prev = Some(Prev {
            start: cur.start,
            mask: cur.mask,
            last_up,
        });
        obs
    }
}

/// The daemon's running measurements. Counts and millisecond differences only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Strokes finished, any kind.
    pub strokes: u32,
    /// Two-hand strokes by (right minus left) first key down, `-60..=60` ms in 1 ms buckets.
    pub diff: Vec<u32>,
    /// Two-hand strokes outside that range (not one chord pressed together).
    pub diff_out: u32,
    /// Split suspects: opposite-half strokes started within 40 ms of each other.
    pub split: u32,
    /// Split suspects by the half of the first stroke and the delay that would have joined them
    /// (`fix[side][ms]`, ms 1..=50).
    pub fix: [Vec<u32>; 2],
    /// Strokes of each half by how long after that half's last key up the rest of the stroke
    /// ended (`gap[side][ms]`, 0..=50). Delaying that half costs `delay - gap` on these.
    pub gap: [Vec<u32>; 2],
    /// Strokes where that half's last key up ended the stroke (or was the whole stroke).
    pub gap_last: [u32; 2],
    /// The delay the engine ran with when the recent strokes were measured.
    pub delay_ms: u32,
    /// Latency added to each of the last strokes, in ms.
    pub recent: VecDeque<u8>,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            strokes: 0,
            diff: vec![0; usize::try_from(2 * DIFF_RANGE + 1).unwrap_or(121)],
            diff_out: 0,
            split: 0,
            fix: [vec![0; GAP_MAX + 1], vec![0; GAP_MAX + 1]],
            gap: [vec![0; GAP_MAX + 1], vec![0; GAP_MAX + 1]],
            gap_last: [0; 2],
            delay_ms: 0,
            recent: VecDeque::new(),
        }
    }
}

/// What delaying one half by some time would cost, from [`Stats::predict`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cost {
    /// Share of all strokes that wait.
    pub share: f64,
    /// Mean wait over all strokes, ms.
    pub mean: f64,
    /// Longest wait, ms.
    pub max: u32,
}

/// A suggestion: which half, how long, and what is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Suggestion {
    pub side: usize,
    pub delay_ms: u32,
    /// Split suspects still there at that delay.
    pub remaining: u32,
    /// Sized by the offset between the halves, not by split strokes (too few of them).
    pub by_offset: bool,
}

/// Percentile of the first-down differences, in ms.
fn percentile(buckets: &[u32], total: u32, fraction: f64) -> i64 {
    let target = (f64::from(total) * fraction).ceil().max(1.0);
    let mut seen = 0.0;
    for (i, &count) in buckets.iter().enumerate() {
        seen += f64::from(count);
        if seen >= target {
            return i64::try_from(i).unwrap_or(0) - DIFF_RANGE;
        }
    }
    0
}

impl Stats {
    pub fn record(&mut self, obs: &Obs, delay_ms: u32) {
        self.strokes = self.strokes.saturating_add(1);
        if let Some(diff) = obs.diff_ms {
            if diff.abs() <= DIFF_RANGE {
                let index = usize::try_from(diff + DIFF_RANGE).unwrap_or(0);
                self.diff[index] = self.diff[index].saturating_add(1);
            } else {
                self.diff_out = self.diff_out.saturating_add(1);
            }
        }
        for (side, gap) in obs.gap_ms.iter().enumerate() {
            match gap {
                Some(Some(ms)) if (*ms as usize) <= GAP_MAX => {
                    self.gap[side][*ms as usize] += 1;
                }
                // The gap is longer than any delay: waiting never changes this stroke
                Some(Some(_)) => {}
                Some(None) => self.gap_last[side] += 1,
                None => {}
            }
        }
        if let Some((side, needed)) = obs.split {
            self.split = self.split.saturating_add(1);
            if (needed as usize) <= GAP_MAX {
                self.fix[side][needed as usize] += 1;
            }
        }
        self.delay_ms = delay_ms;
        if delay_ms > 0 {
            self.recent
                .push_back(u8::try_from(obs.added_ms).unwrap_or(u8::MAX));
            while self.recent.len() > RECENT {
                self.recent.pop_front();
            }
        } else {
            self.recent.clear();
        }
    }

    /// Two-hand strokes counted.
    #[must_use]
    pub fn samples(&self) -> u32 {
        self.diff.iter().sum()
    }

    #[must_use]
    pub fn median(&self) -> i64 {
        percentile(&self.diff, self.samples(), 0.5)
    }

    #[must_use]
    pub fn quartiles(&self) -> (i64, i64) {
        let n = self.samples();
        (
            percentile(&self.diff, n, 0.25),
            percentile(&self.diff, n, 0.75),
        )
    }

    #[must_use]
    pub fn mean(&self) -> f64 {
        let n = self.samples();
        if n == 0 {
            return 0.0;
        }
        let sum: f64 = self
            .diff
            .iter()
            .enumerate()
            .map(|(i, &c)| (i as f64 - DIFF_RANGE as f64) * f64::from(c))
            .sum();
        sum / f64::from(n)
    }

    /// What delaying half `side` (0 left, 1 right) by `delay_ms` costs, over every stroke so far.
    #[must_use]
    pub fn predict(&self, side: usize, delay_ms: u32) -> Cost {
        if delay_ms == 0 {
            return Cost {
                share: 0.0,
                mean: 0.0,
                max: 0,
            };
        }
        let mut waits = f64::from(self.gap_last[side]) * f64::from(delay_ms);
        let mut share = f64::from(self.gap_last[side]);
        let mut max = if self.gap_last[side] > 0 { delay_ms } else { 0 };
        for (gap, &count) in self.gap[side].iter().enumerate() {
            let gap = u32::try_from(gap).unwrap_or(0);
            if count == 0 || gap >= delay_ms {
                continue;
            }
            let wait = delay_ms - gap;
            waits = f64::from(count).mul_add(f64::from(wait), waits);
            share += f64::from(count);
            max = max.max(wait);
        }
        let n = f64::from(self.strokes.max(1));
        Cost {
            share: share / n,
            mean: waits / n,
            max,
        }
    }

    /// Split suspects left when half `side` is delayed by `delay_ms`.
    #[must_use]
    pub fn remaining_at(&self, side: usize, delay_ms: u32) -> u32 {
        let fixed: u32 = self.fix[side]
            .iter()
            .enumerate()
            .filter(|(needed, _)| *needed >= 1 && *needed as u32 <= delay_ms)
            .map(|(_, &c)| c)
            .sum();
        self.split.saturating_sub(fixed)
    }

    /// The smallest delay that joins at least 90% of the split suspects the delayed half can
    /// fix, or, with too few of those, the offset between the halves.
    #[must_use]
    pub fn suggest(&self) -> Option<Suggestion> {
        const MIN_SUSPECTS: u32 = 3;
        let fixable = |side: usize| self.fix[side].iter().sum::<u32>();
        let side = if fixable(0) == fixable(1) {
            usize::from(self.median() < 0)
        } else {
            usize::from(fixable(1) > fixable(0))
        };
        let total = fixable(side);
        if total >= MIN_SUSPECTS {
            let target = (f64::from(total) * 0.9).ceil();
            let mut seen = 0.0;
            for (needed, &count) in self.fix[side].iter().enumerate() {
                seen += f64::from(count);
                if seen >= target {
                    let delay_ms = u32::try_from(needed).unwrap_or(1);
                    return Some(Suggestion {
                        side,
                        delay_ms,
                        remaining: self.remaining_at(side, delay_ms),
                        by_offset: false,
                    });
                }
            }
        }
        let median = self.median();
        if self.samples() >= ENOUGH && median.abs() >= 2 {
            let delay_ms = u32::try_from(median.abs())
                .unwrap_or(1)
                .min(MAX_SKEW_DELAY_MS);
            let side = usize::from(median < 0);
            return Some(Suggestion {
                side,
                delay_ms,
                remaining: self.remaining_at(side, delay_ms),
                by_offset: true,
            });
        }
        None
    }

    /// Share, mean and max of the latency recent strokes actually had added.
    #[must_use]
    pub fn recent_cost(&self) -> Option<(usize, Cost)> {
        let n = self.recent.len();
        if n == 0 {
            return None;
        }
        let delayed = self.recent.iter().filter(|&&ms| ms > 0).count();
        let sum: u32 = self.recent.iter().map(|&ms| u32::from(ms)).sum();
        let max = self.recent.iter().copied().max().map_or(0, u32::from);
        Some((
            n,
            Cost {
                share: delayed as f64 / n as f64,
                mean: f64::from(sum) / n as f64,
                max,
            },
        ))
    }

    /// The text `keymux steno skew` prints.
    #[must_use]
    pub fn report(&self) -> String {
        use std::fmt::Write;
        let n = self.samples();
        let mut out = String::new();
        let _ = writeln!(out, "Two-hand strokes measured: {n}");
        if n > 0 {
            let (q1, q3) = self.quartiles();
            let _ = writeln!(
                out,
                "  right minus left, first key down, as the keyboard reports it (before any skew delay):"
            );
            let _ = writeln!(
                out,
                "  median {:+} ms   mean {:+.1} ms   25th..75th percentile {q1:+} .. {q3:+} ms",
                self.median(),
                self.mean()
            );
            if self.diff_out > 0 {
                let _ = writeln!(
                    out,
                    "  ({} more differed by over {DIFF_RANGE} ms and are left out)",
                    self.diff_out
                );
            }
        }
        let _ = writeln!(
            out,
            "Split suspects (a stroke of one half, then one of the other, started within {} ms): {}",
            SPLIT_WINDOW.as_millis(),
            self.split
        );
        match self.recent_cost() {
            Some((count, cost)) => {
                let _ = writeln!(
                    out,
                    "Current setting (delay_ms {}): added latency over the last {count} strokes: {:.0}% of strokes delayed, mean {:.1} ms, max {} ms",
                    self.delay_ms,
                    cost.share * 100.0,
                    cost.mean,
                    cost.max
                );
            }
            None => {
                let _ = writeln!(out, "Current setting: no delay, no added latency");
            }
        }
        match self.suggest() {
            Some(s) => {
                let (name, half) = if s.side == 0 {
                    ("left", "Left")
                } else {
                    ("right", "Right")
                };
                let how = if s.by_offset {
                    "the median offset (too few split strokes to size it by)"
                } else {
                    "the smallest delay that joins 90% of the split strokes it can fix"
                };
                let cost = self.predict(s.side, s.delay_ms);
                let _ = writeln!(
                    out,
                    "Suggested: delay the {name} half by {} ms ({how}).",
                    s.delay_ms
                );
                let _ = writeln!(
                    out,
                    "  Split suspects left at that value: {} of {}",
                    s.remaining, self.split
                );
                let _ = writeln!(
                    out,
                    "  Predicted added latency from the strokes so far: {:.0}% of strokes delayed, mean {:.1} ms, max {} ms; strokes ending on the {} half wait nothing",
                    cost.share * 100.0,
                    cost.mean,
                    cost.max,
                    if s.side == 0 { "right" } else { "left" }
                );
                let _ = writeln!(out, "Config line, in the steno layer's settings:");
                let _ = writeln!(out, "  skew: (delay_ms: {}, half: {half}),", s.delay_ms);
            }
            None if n >= ENOUGH => {
                let _ = writeln!(out, "No skew detected: the halves line up (median offset under 2 ms) and few strokes split.");
            }
            None => {
                let _ = writeln!(
                    out,
                    "No skew detected; need {} more two-hand strokes.",
                    ENOUGH - n
                );
            }
        }
        let _ = writeln!(
            out,
            "A shifted median estimates the radio lag: human timing is noisy but symmetric, so it\naverages out over a few hundred two-hand strokes. Keep typing normally and run this again."
        );
        out
    }
}

/// The daemon's measurements, shared by every keyboard.
pub fn global() -> &'static Mutex<Stats> {
    static STATS: std::sync::OnceLock<Mutex<Stats>> = std::sync::OnceLock::new();
    STATS.get_or_init(Mutex::default)
}

/// Add a finished stroke to the daemon's measurements.
pub fn record(obs: &Obs, delay_ms: u32) {
    global()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(obs, delay_ms);
}

/// A copy of the daemon's measurements, cleared when `reset`.
pub fn take_snapshot(reset: bool) -> Stats {
    let mut stats = global().lock().unwrap_or_else(PoisonError::into_inner);
    let copy = stats.clone();
    if reset {
        *stats = Stats::default();
    }
    copy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keycode::KeyCode;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn keys() -> HashMap<KeyCode, u32> {
        super::super::layout::build_key_map(
            &super::super::layout::preset("qwerty").unwrap(),
            &HashMap::new(),
        )
        .unwrap()
    }

    #[test]
    fn hands_come_from_the_layout() {
        let layout = Layout::plover();
        let classes = classify(&layout, &keys());
        assert_eq!(classes[&KeyCode::KC_Q], Class::Left); // S-
        assert_eq!(classes[&KeyCode::KC_C], Class::Left); // A-
        assert_eq!(classes[&KeyCode::KC_V], Class::Left); // O-
        assert_eq!(classes[&KeyCode::KC_T], Class::Center); // *
        assert_eq!(classes[&KeyCode::KC_N], Class::Right); // -E
        assert_eq!(classes[&KeyCode::KC_M], Class::Right); // -U
        assert_eq!(classes[&KeyCode::KC_I], Class::Right);
        assert_eq!(classes[&KeyCode::KC_1], Class::Center); // number key
    }

    #[test]
    fn config_selects_a_half_or_keys_and_rejects_the_rest() {
        let layout = Layout::plover();
        let keys = keys();
        let left = SkewConfig {
            delay_ms: 8,
            half: SkewSide::Left,
            keys: vec![],
        };
        let set = delayed_keys(&left, &layout, &keys).unwrap();
        assert!(set.contains(&KeyCode::KC_Q) && !set.contains(&KeyCode::KC_N));
        assert_eq!(set.len(), 10);
        let explicit = SkewConfig {
            delay_ms: 8,
            half: SkewSide::None,
            keys: vec![KeyCode::KC_Q, KeyCode::KC_T],
        };
        assert_eq!(delayed_keys(&explicit, &layout, &keys).unwrap().len(), 2);
        assert!(check_config(&left, &layout, &keys).is_ok());
        let too_long = SkewConfig {
            delay_ms: 51,
            ..left
        };
        assert!(check_config(&too_long, &layout, &keys)
            .unwrap_err()
            .contains("50 ms"));
        let unknown = SkewConfig {
            delay_ms: 5,
            half: SkewSide::None,
            keys: vec![KeyCode::KC_F1],
        };
        assert!(check_config(&unknown, &layout, &keys)
            .unwrap_err()
            .contains("not a stroke key"));
        let empty = SkewConfig {
            delay_ms: 5,
            ..SkewConfig::default()
        };
        assert!(check_config(&empty, &layout, &keys)
            .unwrap_err()
            .contains("select no stroke keys"));
        assert!(check_config(&SkewConfig::default(), &layout, &keys).is_ok());
    }

    fn tracker(delay: u32) -> Tracker {
        let layout = Layout::plover();
        let keys = keys();
        let cfg = SkewConfig {
            delay_ms: delay,
            half: SkewSide::Left,
            keys: vec![],
        };
        let delayed = delayed_keys(&cfg, &layout, &keys).unwrap();
        Tracker::new(classify(&layout, &keys), delayed, ms(u64::from(delay)))
    }

    /// One chord: left key `l` down at 0 for `l_hold` ms, right key `r` down at `r_at` for `r_hold` ms.
    fn chord(t: &mut Tracker, base: Instant, l_hold: u64, r_at: u64, r_hold: u64) -> Obs {
        let (l, r) = (KeyCode::KC_Q, KeyCode::KC_N);
        let mut events = vec![
            (0, l, true),
            (l_hold, l, false),
            (r_at, r, true),
            (r_at + r_hold, r, false),
        ];
        events.sort_by_key(|e| (e.0, e.2));
        let mut last = None;
        for (at, key, down) in events {
            last = t.event(key, down, base + ms(at)).or(last);
        }
        last.unwrap()
    }

    #[test]
    fn a_chord_is_measured_right_minus_left() {
        let mut t = tracker(0);
        let obs = chord(&mut t, Instant::now(), 60, 7, 60);
        assert_eq!(obs.diff_ms, Some(7));
        assert_eq!(obs.split, None);
    }

    #[test]
    fn a_split_pair_is_a_suspect_and_says_what_would_join_it() {
        let mut t = tracker(0);
        let base = Instant::now();
        let first = chord_one(&mut t, base, KeyCode::KC_Q, 0, 5);
        assert_eq!(first.split, None);
        let second = chord_one(&mut t, base, KeyCode::KC_N, 12, 5);
        // The left key was up at 5 and the right came down at 12: a delay of 8 ms joins them
        assert_eq!(second.split, Some((0, 8)));
        // Starting 41 ms apart is no suspect
        let third = chord_one(&mut t, base, KeyCode::KC_Q, 100, 105);
        assert_eq!(third.split, None);
        let fourth = chord_one(&mut t, base, KeyCode::KC_N, 141, 150);
        assert_eq!(fourth.split, None);
    }

    fn chord_one(t: &mut Tracker, base: Instant, key: KeyCode, down: u64, up: u64) -> Obs {
        assert_eq!(t.event(key, true, base + ms(down)), None);
        t.event(key, false, base + ms(up)).unwrap()
    }

    #[test]
    fn centered_and_shifted_distributions_tell_apart() {
        let mut centered = Stats::default();
        let mut shifted = Stats::default();
        // A symmetric spread around 0, and the same around +8 (the right half lags)
        for i in 0..400_i64 {
            let jitter = (i % 9) - 4;
            centered.record(
                &Obs {
                    diff_ms: Some(jitter),
                    ..Obs::default()
                },
                0,
            );
            shifted.record(
                &Obs {
                    diff_ms: Some(8 + jitter),
                    ..Obs::default()
                },
                0,
            );
        }
        assert_eq!(centered.median(), 0);
        assert_eq!(shifted.median(), 8);
        assert!((shifted.mean() - 8.0).abs() < 0.2);
        let (q1, q3) = shifted.quartiles();
        assert!(q1 < 8 && q3 > 8);
        assert_eq!(centered.suggest(), None);
        let s = shifted.suggest().unwrap();
        assert_eq!((s.side, s.delay_ms, s.by_offset), (0, 8, true));
        assert!(shifted.report().contains("delay_ms: 8, half: Left"));
        let few = Stats::default();
        assert!(few.report().contains("need 200 more"));
    }

    #[test]
    fn the_suggestion_is_the_smallest_delay_that_joins_the_split_strokes() {
        let mut stats = Stats::default();
        // Nine suspects need 6..=8 ms, one outlier needs 30: the 90% mark is 8, not 30
        for needed in [6, 6, 7, 7, 7, 8, 8, 8, 8, 30] {
            stats.record(
                &Obs {
                    split: Some((0, needed)),
                    ..Obs::default()
                },
                0,
            );
        }
        let s = stats.suggest().unwrap();
        assert_eq!(
            (s.side, s.delay_ms, s.remaining, s.by_offset),
            (0, 8, 1, false)
        );
        assert_eq!(stats.remaining_at(0, 7), 5);
        // Suspects whose first stroke was on the other half stay, whatever the delay
        stats.record(
            &Obs {
                split: Some((1, 5)),
                ..Obs::default()
            },
            0,
        );
        assert_eq!(stats.remaining_at(0, 50), 1);
    }

    #[test]
    fn only_strokes_that_end_on_the_delayed_half_wait() {
        let mut t = tracker(10);
        let base = Instant::now();
        // The left (delayed) half lets go last: the stroke waits the whole delay
        assert_eq!(chord(&mut t, base, 100, 20, 30).added_ms, 10);
        // The right half lets go well after the delayed left would be replayed: no wait
        assert_eq!(chord(&mut t, base, 10, 5, 60).added_ms, 0);
        // The left is replayed 7 ms before the right lets go... no: 3 ms after it
        assert_eq!(chord(&mut t, base, 62, 5, 60).added_ms, 7);
        // A stroke of the delayed half alone waits; one of the other half alone does not
        assert_eq!(chord_one(&mut t, base, KeyCode::KC_Q, 0, 20).added_ms, 10);
        assert_eq!(chord_one(&mut t, base, KeyCode::KC_N, 0, 20).added_ms, 0);
    }

    #[test]
    fn the_prediction_follows_the_gaps() {
        let mut stats = Stats::default();
        let base = Instant::now();
        let mut t = tracker(0);
        // Left ends first by 3 ms, left ends first by 20 ms, left alone, right alone
        for obs in [
            chord(&mut t, base, 57, 5, 55),
            chord(&mut t, base, 40, 5, 55),
            chord_one(&mut t, base, KeyCode::KC_Q, 0, 20),
            chord_one(&mut t, base, KeyCode::KC_N, 0, 20),
        ] {
            stats.record(&obs, 0);
        }
        let cost = stats.predict(0, 8);
        // Waits of 5 (gap 3), 0 (gap 20), 8 and 0
        assert_eq!(cost.max, 8);
        assert!((cost.share - 0.5).abs() < 1e-9);
        // Delaying the right half instead: it ends the two chords and is alone once
        assert!((stats.predict(1, 8).share - 0.75).abs() < 1e-9);
        assert_eq!(
            stats.predict(0, 0),
            Cost {
                share: 0.0,
                mean: 0.0,
                max: 0
            }
        );
    }
}
