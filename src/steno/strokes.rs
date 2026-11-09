//! The strokes struck, in order, kept for plugins to read back one at a time.
//!
//! A plugin deciding how to read a word sometimes needs the strokes before it, not only the text
//! (see the "History" section of `docs/plugins/README.md`). They live here, in a [`StrokeLog`]
//! that is part of the shared [`super::journal::History`], so it is fenced, undone and shared
//! exactly as that is, and kept in step with the translations in it (see
//! `Translator::exchange`): the log is the strokes of those translations, in order, however
//! many more than the history keeps undoable.
//!
//! # Shape
//!
//! One flat vector of `u32` stroke bits, indexed by *absolute* index (the count of strokes since
//! the log began, never reused except by undo), shared between the translating thread, which
//! appends and truncates, and a plugin's worker thread, which reads. Nothing is copied per
//! question: a question carries a [`Window`] (a handle on the log, and the index of the run's
//! first stroke), and each read is one lock, one bounds check and one array index, however long
//! the log is. The oldest strokes are dropped from the front a block at a time once the log is
//! over [`CAP`] strokes (4 MiB), so the cost per append is constant on average.
//!
//! # Staleness
//!
//! Appending never changes anything below an index already handed out. Truncating (undo) can,
//! once something else is appended in its place, so every stretch of the log carries a *lineage*
//! number that changes whenever a truncation cuts into it. A question remembers the lineage of
//! its last visible stroke; reads whose log no longer has that lineage find nothing (the answer
//! to that question is thrown away anyway).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Strokes kept at least: the newest this many (1,048,576, 4 MiB of bits).
pub const CAP: usize = 1 << 20;

/// How far over [`CAP`] the log grows before the oldest are dropped, so dropping is rare and its
/// cost, a move of the rest, is spread over this many appends.
const SLACK: usize = CAP / 16;

/// How many of the newest strokes a copy of a log keeps (a translator tried out on a copy, see
/// `Translator::clone`, must stay cheap).
const COPIED: usize = 4096;

/// Which log, where a run starts, and what of the log it may see: the key of a question's view
/// of history. Two questions with the same `Seen` see exactly the same strokes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Seen {
    /// Which log
    log: u64,
    /// Absolute index of the run's first stroke: the stroke before it is `k = 0`
    base: u64,
    /// Absolute index of the oldest stroke visible (after the last wall)
    floor: u64,
    /// The lineage of the stroke just before `base`
    lineage: u64,
}

/// What a plugin may read while it answers one question: the log, and the part of it that
/// precedes the question's run.
#[derive(Debug, Clone)]
pub struct Window {
    log: StrokeLog,
    seen: Seen,
}

impl Window {
    /// How many strokes of history there are before the run.
    #[must_use]
    #[allow(clippy::len_without_is_empty)] // a count a plugin reads, not a collection
    pub fn len(&self) -> i32 {
        self.log.len_seen(&self.seen)
    }

    /// The stroke `k` steps back from the run's first (0 is the one right before), or `-1`.
    #[must_use]
    pub fn stroke(&self, k: i32) -> i64 {
        self.log.get(&self.seen, k)
    }

    /// What identifies this view of history in a question.
    #[must_use]
    pub const fn seen(&self) -> &Seen {
        &self.seen
    }
}

#[derive(Debug)]
struct Inner {
    /// Unique among logs
    id: u64,
    /// Absolute index of `data[0]`
    first: u64,
    data: Vec<u32>,
    /// Absolute indices, ascending: strokes before the last one at or below an index are hidden
    /// from a run starting there
    walls: Vec<u64>,
    /// `(start, lineage)`, `start` ascending: stroke `i` has the lineage of the last entry whose
    /// start is at most `i`, or 0 before the first
    eras: Vec<(u64, u64)>,
    last_lineage: u64,
    cap: usize,
    slack: usize,
}

impl Inner {
    const fn end(&self) -> u64 {
        self.first + self.data.len() as u64
    }

    fn lineage_at(&self, i: u64) -> u64 {
        let after = self.eras.partition_point(|&(start, _)| start <= i);
        after.checked_sub(1).map_or(0, |at| self.eras[at].1)
    }

    fn floor_for(&self, base: u64) -> u64 {
        let at = self.walls.partition_point(|&w| w <= base);
        at.checked_sub(1)
            .map_or(self.first, |at| self.walls[at].max(self.first))
    }

    /// Drop all but the newest `cap` strokes.
    fn trim(&mut self) {
        let drop = self.data.len().saturating_sub(self.cap);
        if drop == 0 {
            return;
        }
        self.data.drain(..drop);
        self.first += drop as u64;
        let first = self.first;
        self.walls.retain(|&w| w >= first);
        let live = self.eras.partition_point(|&(start, _)| start <= first);
        if live > 1 {
            self.eras.drain(..live - 1);
        }
    }

    fn truncate_to(&mut self, to: u64) {
        if to >= self.end() {
            return;
        }
        let keep = usize::try_from(to.saturating_sub(self.first)).unwrap_or(0);
        self.data.truncate(keep);
        if to < self.first {
            self.first = to;
        }
        while self.eras.last().is_some_and(|&(start, _)| start >= to) {
            self.eras.pop();
        }
        self.last_lineage += 1;
        self.eras.push((to, self.last_lineage));
        while self.walls.last().is_some_and(|&w| w > to) {
            self.walls.pop();
        }
    }
}

#[derive(Debug)]
struct Shared {
    inner: RwLock<Inner>,
    /// How many reads plugins have made (a count for tests)
    reads: AtomicU64,
}

/// The strokes struck since the last wall (and as far back as [`CAP`] allows), shared between
/// the thread translating and the threads of plugins reading.
#[derive(Debug, Clone)]
pub struct StrokeLog {
    shared: Arc<Shared>,
}

impl Default for StrokeLog {
    fn default() -> Self {
        Self::with_cap(CAP, SLACK)
    }
}

impl StrokeLog {
    /// A deep copy of the newest strokes, in a log of its own (not the same view, not shared).
    #[must_use]
    pub fn copy(&self) -> Self {
        let from = self.read();
        let keep = from.data.len().min(COPIED);
        let log = Self::with_cap(from.cap, from.slack);
        {
            let mut to = log.write();
            to.first = from.end() - keep as u64;
            to.data
                .extend_from_slice(&from.data[from.data.len() - keep..]);
            let first = to.first;
            to.walls = from.walls.iter().copied().filter(|&w| w >= first).collect();
        }
        log
    }
}

impl StrokeLog {
    /// An empty log keeping at least the newest `cap` strokes, dropping `slack` at a time.
    #[must_use]
    pub fn with_cap(cap: usize, slack: usize) -> Self {
        static IDS: AtomicU64 = AtomicU64::new(1);
        Self {
            shared: Arc::new(Shared {
                inner: RwLock::new(Inner {
                    id: IDS.fetch_add(1, Ordering::Relaxed),
                    first: 0,
                    data: Vec::new(),
                    walls: Vec::new(),
                    eras: Vec::new(),
                    last_lineage: 0,
                    cap,
                    slack,
                }),
                reads: AtomicU64::new(0),
            }),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.shared.inner.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.shared.inner.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Append strokes struck.
    pub fn push(&self, strokes: &[u32]) {
        if strokes.is_empty() {
            return;
        }
        let mut log = self.write();
        log.data.extend_from_slice(strokes);
        if log.data.len() >= log.cap + log.slack {
            log.trim();
        }
    }

    /// Take back the newest `count` strokes (undo).
    pub fn pop(&self, count: usize) {
        if count == 0 {
            return;
        }
        let mut log = self.write();
        let to = log.end().saturating_sub(count as u64);
        log.truncate_to(to);
    }

    /// Nothing before here is history any more: a key press, text typed by hand, a barrier.
    pub fn wall(&self) {
        let mut log = self.write();
        let end = log.end();
        if log.walls.last() != Some(&end) {
            log.walls.push(end);
        }
    }

    /// A wall at absolute index `at` (strokes before it hidden from a run starting at or after
    /// it), if `at` is in the log.
    pub fn wall_at(&self, at: u64) {
        let mut log = self.write();
        if at > log.end() {
            return;
        }
        let place = log.walls.partition_point(|&w| w < at);
        if log.walls.get(place) != Some(&at) {
            log.walls.insert(place, at);
        }
    }

    /// The absolute index the next stroke will have.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.read().end()
    }

    /// The view of the log for a run whose first stroke has absolute index `base`.
    #[must_use]
    pub fn window(&self, base: u64) -> Window {
        let log = self.read();
        let base = base.min(log.end());
        let lineage = base.checked_sub(1).map_or(0, |last| log.lineage_at(last));
        Window {
            seen: Seen {
                log: log.id,
                base,
                floor: log.floor_for(base),
                lineage,
            },
            log: self.clone(),
        }
    }

    /// How many strokes `seen` can read: 0 if the log has since been cut under it.
    fn len_seen(&self, seen: &Seen) -> i32 {
        self.shared.reads.fetch_add(1, Ordering::Relaxed);
        let (holds, first) = {
            let log = self.read();
            (Self::holds(&log, seen), log.first)
        };
        if !holds {
            return 0;
        }
        let from = seen.floor.max(first);
        i32::try_from(seen.base.saturating_sub(from)).unwrap_or(i32::MAX)
    }

    /// The stroke `k` before the run `seen` is of, or -1.
    fn get(&self, seen: &Seen, k: i32) -> i64 {
        self.shared.reads.fetch_add(1, Ordering::Relaxed);
        let Ok(k) = u64::try_from(k) else {
            return -1;
        };
        let Some(index) = seen.base.checked_sub(k + 1) else {
            return -1;
        };
        let log = self.read();
        if index < seen.floor || index < log.first || !Self::holds(&log, seen) {
            return -1;
        }
        usize::try_from(index - log.first)
            .ok()
            .and_then(|at| log.data.get(at))
            .map_or(-1, |&bits| i64::from(bits))
    }

    /// Whether everything before `seen`'s run is still in the log as it was.
    fn holds(log: &Inner, seen: &Seen) -> bool {
        log.id == seen.log
            && seen.base <= log.end()
            && seen
                .base
                .checked_sub(1)
                .is_none_or(|last| log.lineage_at(last) == seen.lineage)
    }

    /// How many reads plugins made of this log.
    #[must_use]
    pub fn reads(&self) -> u64 {
        self.shared.reads.load(Ordering::Relaxed)
    }

    /// How many strokes are held, from the oldest still kept.
    #[must_use]
    pub fn kept(&self) -> usize {
        self.read().data.len()
    }
}

impl PartialEq for Window {
    fn eq(&self, other: &Self) -> bool {
        self.seen == other.seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_of(strokes: &[u32]) -> StrokeLog {
        let log = StrokeLog::default();
        log.push(strokes);
        log
    }

    #[test]
    fn k_counts_back_from_before_the_run() {
        let log = log_of(&[10, 11, 12, 13]);
        let w = log.window(3);
        assert_eq!(w.len(), 3);
        assert_eq!(
            (0..5).map(|k| w.stroke(k)).collect::<Vec<_>>(),
            [12, 11, 10, -1, -1]
        );
        assert_eq!(w.stroke(-1), -1);
        assert_eq!(log.window(0).len(), 0);
    }

    #[test]
    fn strokes_after_the_question_change_nothing_below_it() {
        let log = log_of(&[1, 2, 3]);
        let w = log.window(2);
        log.push(&[4, 5]);
        assert_eq!(
            (w.len(), w.stroke(0), w.stroke(1), w.stroke(2)),
            (2, 2, 1, -1)
        );
    }

    #[test]
    fn a_wall_hides_what_is_before_it() {
        let log = log_of(&[1, 2, 3]);
        log.wall();
        log.push(&[4, 5]);
        assert_eq!(log.window(4).len(), 1);
        assert_eq!(log.window(4).stroke(0), 4);
        assert_eq!(log.window(4).stroke(1), -1);
        assert_eq!(
            log.window(3).len(),
            0,
            "the run right after the wall sees none"
        );
        assert_eq!(
            log.window(2).len(),
            2,
            "a run before the wall still sees back"
        );
    }

    #[test]
    fn undo_cutting_under_a_question_leaves_it_nothing_to_read_even_after_new_strokes() {
        let log = log_of(&[1, 2, 3, 4]);
        let w = log.window(3);
        log.pop(2);
        assert_eq!(w.len(), 0);
        assert_eq!(w.stroke(0), -1);
        log.push(&[8, 9]);
        assert_eq!((w.len(), w.stroke(0), w.stroke(2)), (0, -1, -1));
        // A new question sees the new strokes
        assert_eq!(log.window(3).stroke(0), 8);
        assert_eq!(log.window(3).stroke(2), 1);
    }

    #[test]
    fn taking_back_what_the_run_itself_struck_keeps_the_question_good() {
        let log = log_of(&[1, 2, 3]);
        let w = log.window(2);
        log.pop(1);
        log.push(&[3]);
        assert_eq!((w.len(), w.stroke(0)), (2, 2));
    }

    #[test]
    fn a_wall_the_undo_removes_is_gone() {
        let log = log_of(&[1, 2]);
        log.push(&[3]);
        log.wall();
        log.pop(1);
        log.push(&[4, 5]);
        assert_eq!(log.window(4).len(), 4, "no wall any more");
    }

    #[test]
    fn the_oldest_are_dropped_a_block_at_a_time_and_the_newest_cap_stay() {
        let log = StrokeLog::with_cap(8, 4);
        for i in 0..40u32 {
            log.push(&[i]);
        }
        assert!(log.kept() >= 8 && log.kept() < 12, "{}", log.kept());
        let w = log.window(40);
        assert_eq!(w.stroke(0), 39);
        assert_eq!(w.stroke(7), 32);
        assert_eq!(w.stroke(i32::try_from(log.kept()).unwrap()), -1);
        assert_eq!(w.len(), i32::try_from(log.kept()).unwrap());
    }

    #[test]
    fn a_copy_is_its_own_log_with_the_newest_strokes() {
        let log = log_of(&[1, 2, 3]);
        let copy = log.copy();
        copy.push(&[4]);
        assert_eq!(log.end(), 3);
        assert_eq!(copy.end(), 4);
        assert_eq!(copy.window(4).stroke(3), 1);
        assert_eq!(log.window(3).stroke(0), 3);
        assert_ne!(log.window(3).seen(), copy.window(3).seen());
    }

    #[test]
    fn the_default_keeps_the_newest_1048576() {
        let log = StrokeLog::default();
        let all: Vec<u32> = (0..u32::try_from(CAP + SLACK).unwrap()).collect();
        log.push(&all);
        assert_eq!(log.kept(), CAP);
        let w = log.window(log.end());
        assert_eq!(w.len(), i32::try_from(CAP).unwrap());
        assert_eq!(w.stroke(0), i64::try_from(CAP + SLACK - 1).unwrap());
        assert_eq!(
            w.stroke(i32::try_from(CAP).unwrap() - 1),
            i64::try_from(SLACK).unwrap()
        );
        assert_eq!(w.stroke(i32::try_from(CAP).unwrap()), -1);
    }

    #[test]
    fn reads_are_counted() {
        let log = log_of(&[1, 2, 3]);
        let w = log.window(3);
        let _ = (w.len(), w.stroke(0), w.stroke(9));
        assert_eq!(log.reads(), 3);
    }
}
