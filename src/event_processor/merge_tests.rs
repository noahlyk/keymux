//! A keyboard group, end to end without devices: scripted members merged into one stream, the
//! rules for a key two members hold, members that come and go, and the keymap behind it seeing
//! one keyboard (a steno chord, a layer, a one-shot and a mod-tap that span members).

use super::emission_tests::{key_at, Fixture, Harness, Recorder};
use super::merge::{Arrival, Member, Merge};
use super::pump::{Source, Step};
use crossbeam_channel::{unbounded, Sender};
use evdev::{EventType, InputEvent, Key};
use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::os::unix::io::RawFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

/// A member whose reads are scripted: each read hands over the next batch, then `then` (a
/// device that vanished, say) or nothing.
struct Script {
    batches: VecDeque<Vec<InputEvent>>,
    then: Option<io::ErrorKind>,
    released: Rc<Cell<bool>>,
}

impl Script {
    fn of(batches: Vec<Vec<InputEvent>>) -> Self {
        Self {
            batches: batches.into(),
            then: None,
            released: Rc::default(),
        }
    }
}

impl Member for Script {
    fn read(&mut self, into: &mut Vec<InputEvent>) -> io::Result<()> {
        if let Some(batch) = self.batches.pop_front() {
            into.extend(batch);
            return Ok(());
        }
        Err(self.then.unwrap_or(io::ErrorKind::WouldBlock).into())
    }

    fn poll_fd(&self) -> RawFd {
        -1
    }

    fn release(&mut self) {
        self.released.set(true);
    }
}

/// Moments, `ms` after a base a few seconds ago (so every one is in the past, as read events are).
struct Clock(SystemTime);

impl Clock {
    fn new() -> Self {
        // Whole microseconds, as the kernel stamps, so a stamp is never a hair before its base
        let base = SystemTime::now() - Duration::from_secs(5);
        let since = base.duration_since(SystemTime::UNIX_EPOCH).unwrap();
        Self(SystemTime::UNIX_EPOCH + Duration::from_micros(u64::try_from(since.as_micros()).unwrap()))
    }

    fn at(&self, ms: u64) -> SystemTime {
        self.0 + Duration::from_millis(ms)
    }
}

fn path(n: u32) -> PathBuf {
    PathBuf::from(format!("/dev/input/event{n}"))
}

/// A merge of `members` (member `n` is `/dev/input/event{n}`), and the end that sends arrivals.
fn merge_of(members: Vec<Script>) -> (Merge<Script>, Sender<Arrival<Script>>) {
    let (tx, rx) = unbounded();
    let mut merge = Merge::new(rx);
    for (n, member) in members.into_iter().enumerate() {
        assert!(merge.join(path(u32::try_from(n).unwrap()), member).is_ok());
    }
    (merge, tx)
}

/// What one read of the merge hands the pump: (type, code, value) and the stamp in ms after `clock`.
fn read(merge: &mut Merge<Script>, clock: &Clock) -> Vec<(EventType, u16, i32, u128)> {
    let mut into = Vec::new();
    match Source::read(merge, &mut into) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
        Err(e) => panic!("{e}"),
    }
    into.iter()
        .map(|e| {
            let ms = e
                .timestamp()
                .duration_since(clock.at(0))
                .map_or(u128::MAX, |d| d.as_millis());
            (e.event_type(), e.code(), e.value(), ms)
        })
        .collect()
}

/// The key events of `read`, as (code, value, ms).
fn keys(events: &[(EventType, u16, i32, u128)]) -> Vec<(u16, i32, u128)> {
    events
        .iter()
        .filter(|e| e.0 == EventType::KEY)
        .map(|&(_, code, value, ms)| (code, value, ms))
        .collect()
}

const SPACE: Key = Key::KEY_SPACE;

#[test]
fn members_are_merged_by_the_kernels_timestamps_and_each_keeps_its_own_order() {
    let clock = Clock::new();
    let a = Script::of(vec![vec![
        key_at(Key::KEY_A, 1, clock.at(10)),
        key_at(Key::KEY_A, 0, clock.at(50)),
        key_at(Key::KEY_B, 1, clock.at(50)),
    ]]);
    let b = Script::of(vec![vec![
        key_at(Key::KEY_C, 1, clock.at(20)),
        key_at(Key::KEY_C, 0, clock.at(30)),
        key_at(Key::KEY_D, 1, clock.at(60)),
    ]]);
    let (mut merge, _tx) = merge_of(vec![a, b]);
    let got = keys(&read(&mut merge, &clock));
    let a_key = Key::KEY_A.code();
    let b_key = Key::KEY_B.code();
    let c = Key::KEY_C.code();
    let d = Key::KEY_D.code();
    assert_eq!(
        got,
        vec![
            (a_key, 1, 10),
            (c, 1, 20),
            (c, 0, 30),
            (a_key, 0, 50),
            (b_key, 1, 50),
            (d, 1, 60)
        ]
    );
}

#[test]
fn two_spacebars_down_down_up_up_is_one_press_at_the_first_down_and_one_release_at_the_last_up() {
    let clock = Clock::new();
    let a = Script::of(vec![vec![
        key_at(SPACE, 1, clock.at(0)),
        key_at(SPACE, 0, clock.at(200)),
    ]]);
    let b = Script::of(vec![vec![
        key_at(SPACE, 1, clock.at(100)),
        key_at(SPACE, 0, clock.at(300)),
    ]]);
    let (mut merge, _tx) = merge_of(vec![a, b]);
    assert_eq!(
        keys(&read(&mut merge, &clock)),
        vec![(SPACE.code(), 1, 0), (SPACE.code(), 0, 300)],
        "A's up leaves B holding it, and B's down changed nothing"
    );
}

#[test]
fn two_spacebars_released_in_the_other_order_release_at_the_last_up_too() {
    let clock = Clock::new();
    let a = Script::of(vec![vec![
        key_at(SPACE, 1, clock.at(0)),
        key_at(SPACE, 0, clock.at(300)),
    ]]);
    let b = Script::of(vec![vec![
        key_at(SPACE, 1, clock.at(100)),
        key_at(SPACE, 0, clock.at(200)),
    ]]);
    let (mut merge, _tx) = merge_of(vec![a, b]);
    assert_eq!(
        keys(&read(&mut merge, &clock)),
        vec![(SPACE.code(), 1, 0), (SPACE.code(), 0, 300)]
    );
}

#[test]
fn a_hold_that_spans_reads_is_still_one_hold() {
    let clock = Clock::new();
    let a = Script::of(vec![
        vec![key_at(SPACE, 1, clock.at(0))],
        vec![key_at(SPACE, 0, clock.at(500))],
    ]);
    let b = Script::of(vec![
        vec![],
        vec![key_at(SPACE, 1, clock.at(100)), key_at(SPACE, 0, clock.at(600))],
    ]);
    let (mut merge, _tx) = merge_of(vec![a, b]);
    assert_eq!(keys(&read(&mut merge, &clock)), vec![(SPACE.code(), 1, 0)]);
    assert_eq!(keys(&read(&mut merge, &clock)), vec![(SPACE.code(), 0, 600)]);
}

#[test]
fn different_keys_on_different_members_do_not_touch_each_other() {
    let clock = Clock::new();
    let a = Script::of(vec![vec![
        key_at(Key::KEY_A, 1, clock.at(0)),
        key_at(Key::KEY_A, 0, clock.at(100)),
    ]]);
    let b = Script::of(vec![vec![
        key_at(Key::KEY_B, 1, clock.at(50)),
        key_at(Key::KEY_B, 0, clock.at(150)),
    ]]);
    let (mut merge, _tx) = merge_of(vec![a, b]);
    let a_key = Key::KEY_A.code();
    let b_key = Key::KEY_B.code();
    assert_eq!(
        keys(&read(&mut merge, &clock)),
        vec![(a_key, 1, 0), (b_key, 1, 50), (a_key, 0, 100), (b_key, 0, 150)]
    );
}

#[test]
fn auto_repeat_and_events_that_are_not_keys_go_on_untouched() {
    let clock = Clock::new();
    let a = Script::of(vec![vec![
        key_at(SPACE, 1, clock.at(0)),
        key_at(SPACE, 2, clock.at(10)),
        InputEvent::new(EventType::SYNCHRONIZATION, 0, 0),
    ]]);
    let (mut merge, _tx) = merge_of(vec![a]);
    let got = read(&mut merge, &clock);
    assert_eq!(got.len(), 3, "{got:?}");
    assert_eq!(got[1].2, 2, "the pump drops the repeat, as it always has");
}

#[test]
fn bounce_is_filtered_per_physical_key_not_across_members() {
    let clock = Clock::new();
    // On one member, a press 10 ms after that key's own release is chatter...
    let bouncing = Script::of(vec![vec![
        key_at(Key::KEY_A, 1, clock.at(0)),
        key_at(Key::KEY_A, 0, clock.at(100)),
        key_at(Key::KEY_A, 1, clock.at(110)),
        key_at(Key::KEY_A, 0, clock.at(120)),
    ]]);
    let (mut merge, _tx) = merge_of(vec![bouncing]);
    let a_key = Key::KEY_A.code();
    assert_eq!(
        keys(&read(&mut merge, &clock)),
        vec![(a_key, 1, 0), (a_key, 0, 100)]
    );
    // ...but the same pair across two members is a real second press
    let first = Script::of(vec![vec![
        key_at(SPACE, 1, clock.at(0)),
        key_at(SPACE, 0, clock.at(100)),
    ]]);
    let second = Script::of(vec![vec![
        key_at(SPACE, 1, clock.at(110)),
        key_at(SPACE, 0, clock.at(200)),
    ]]);
    let (mut merge, _tx) = merge_of(vec![first, second]);
    let space = SPACE.code();
    assert_eq!(
        keys(&read(&mut merge, &clock)),
        vec![(space, 1, 0), (space, 0, 100), (space, 1, 110), (space, 0, 200)]
    );
}

#[test]
fn a_member_that_leaves_holding_a_key_releases_it_unless_another_member_holds_it_too() {
    let clock = Clock::new();
    let a = Script::of(vec![vec![
        key_at(SPACE, 1, clock.at(0)),
        key_at(Key::KEY_A, 1, clock.at(10)),
    ]]);
    let b = Script::of(vec![vec![key_at(SPACE, 1, clock.at(20))]]);
    let (mut merge, tx) = merge_of(vec![a, b]);
    assert_eq!(
        keys(&read(&mut merge, &clock)),
        vec![(SPACE.code(), 1, 0), (Key::KEY_A.code(), 1, 10)]
    );
    // A is unplugged: its A key is released, the space B also holds is not
    tx.send(Arrival::Leave(path(0))).unwrap();
    let got = keys(&read(&mut merge, &clock));
    assert_eq!(
        got.iter().map(|&(code, value, _)| (code, value)).collect::<Vec<_>>(),
        vec![(Key::KEY_A.code(), 0)]
    );
    assert_eq!(merge.len(), 1);
    // And B's own release of it is the one that goes on
    tx.send(Arrival::Leave(path(1))).unwrap();
    let mut into = Vec::new();
    let _ = merge.read(&mut into);
    assert_eq!(
        into.iter().map(|e| (e.code(), e.value())).collect::<Vec<_>>(),
        vec![(SPACE.code(), 0)]
    );
}

#[test]
fn a_device_that_vanishes_is_dropped_and_its_keys_released_and_the_last_one_ends_the_group() {
    let clock = Clock::new();
    let mut gone = Script::of(vec![vec![key_at(Key::KEY_A, 1, clock.at(0))]]);
    gone.then = Some(io::ErrorKind::Other);
    let released = Rc::clone(&gone.released);
    let (mut merge, _tx) = merge_of(vec![gone, Script::of(vec![])]);
    let got = keys(&read(&mut merge, &clock));
    assert_eq!(
        got.iter().map(|&(code, value, _)| (code, value)).collect::<Vec<_>>(),
        vec![(Key::KEY_A.code(), 1)]
    );
    // The next read finds the device gone
    let got = keys(&read(&mut merge, &clock));
    assert_eq!(
        got.iter().map(|&(code, value, _)| (code, value)).collect::<Vec<_>>(),
        vec![(Key::KEY_A.code(), 0)]
    );
    assert!(released.get(), "the dropped device is given back");
    assert_eq!(merge.len(), 1);
    // The last member leaving ends the group the way an unplugged keyboard ends its thread
    merge.leave(&path(1), &mut Vec::new());
    let mut into = Vec::new();
    let error = merge.read(&mut into).unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc::ENODEV));
}

#[test]
fn a_member_that_joins_is_read_from_then_on_and_shares_the_key_state() {
    let clock = Clock::new();
    let a = Script::of(vec![vec![key_at(SPACE, 1, clock.at(0))], vec![
        key_at(SPACE, 0, clock.at(400)),
    ]]);
    let (mut merge, tx) = merge_of(vec![a]);
    assert_eq!(keys(&read(&mut merge, &clock)), vec![(SPACE.code(), 1, 0)]);
    // B joins and presses the space A is holding: nothing changes
    let b = Script::of(vec![vec![
        key_at(SPACE, 1, clock.at(100)),
        key_at(SPACE, 0, clock.at(500)),
        key_at(Key::KEY_B, 1, clock.at(510)),
    ]]);
    assert!(tx.send(Arrival::Join { path: path(1), member: b }).is_ok());
    assert_eq!(
        keys(&read(&mut merge, &clock)),
        vec![(SPACE.code(), 0, 500), (Key::KEY_B.code(), 1, 510)],
        "A released at 400, B still held it until 500"
    );
    assert_eq!(merge.len(), 2);
}

#[test]
fn every_member_is_given_back_on_release_all() {
    let a = Script::of(vec![]);
    let b = Script::of(vec![]);
    let (flag_a, flag_b) = (Rc::clone(&a.released), Rc::clone(&b.released));
    let (mut merge, _tx) = merge_of(vec![a, b]);
    merge.release_all();
    assert!(flag_a.get() && flag_b.get());
}

// What the keymap does with the merged stream

const CHORDS: &str = r#"(
    default_layer: Some("chords"),
    remaps: {},
    layers: {
        "chords": (kind: Steno((dictionaries: ["DICT"])), remaps: {}, disabled_in_game_mode: true),
    },
    game_mode: (remaps: {}),
)"#;

/// The text the keys type through a steno keymap, however the members are split.
fn typed_through(name: &str, events: Vec<Vec<Vec<InputEvent>>>) -> String {
    let fixture = Fixture::new(name);
    let dict = fixture.path().join("main.json");
    std::fs::write(&dict, r#"{"KAT": "cat", "S": "so", "KP": "kp"}"#).unwrap();
    let shared = Mutex::new(fixture.processor_with(&CHORDS.replace("DICT", &dict.display().to_string())));
    let mut harness = Harness::new();
    let (mut merge, _tx) = merge_of(events.into_iter().map(Script::of).collect());
    let mut out = Recorder::default();
    assert_eq!(harness.pump.step(&shared, &mut merge, &mut out).unwrap(), Step::Read);
    // The second pass runs the timeouts; a ring keeps its wait from sleeping
    harness.control.wake();
    let _ = harness.pump.step(&shared, &mut merge, &mut out);
    out.typed().split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn a_steno_chord_with_keys_from_two_members_is_one_stroke() {
    let now = SystemTime::now();
    let ago = |ms: u64| now - Duration::from_millis(ms);
    // S on one member, C and P on the other, pressed together and released together
    let a = vec![vec![key_at(Key::KEY_S, 1, ago(300)), key_at(Key::KEY_S, 0, ago(250))]];
    let b = vec![vec![
        key_at(Key::KEY_C, 1, ago(299)),
        key_at(Key::KEY_P, 1, ago(298)),
        key_at(Key::KEY_C, 0, ago(251)),
        key_at(Key::KEY_P, 0, ago(250)),
    ]];
    assert_eq!(typed_through("chord-two", vec![a, b]), "cat");
}

#[test]
fn a_chord_split_in_time_across_members_is_what_the_single_stream_says() {
    let now = SystemTime::now();
    let ago = |ms: u64| now - Duration::from_millis(ms);
    // S pressed and released on one member BEFORE the other member's C and P go down: no keys
    // are down together, so the stream is two strokes, whichever members they came from
    let events = |split: bool| {
        let first = vec![key_at(Key::KEY_S, 1, ago(400)), key_at(Key::KEY_S, 0, ago(350))];
        let second = vec![
            key_at(Key::KEY_C, 1, ago(300)),
            key_at(Key::KEY_P, 1, ago(299)),
            key_at(Key::KEY_C, 0, ago(250)),
            key_at(Key::KEY_P, 0, ago(249)),
        ];
        if split {
            vec![vec![first], vec![second]]
        } else {
            vec![vec![[first, second].concat()]]
        }
    };
    let one_keyboard = typed_through("split-one", events(false));
    let two_members = typed_through("split-two", events(true));
    assert_eq!(two_members, one_keyboard, "nothing extra is invented by the merge");
    assert!(!two_members.contains("cat"), "{two_members}");
}

/// The text `events` type through a plain (not steno) keymap with `config`, as (code, value).
fn keys_through(name: &str, config: &str, events: Vec<Vec<Vec<InputEvent>>>) -> Vec<(u16, i32)> {
    let fixture = Fixture::new(name);
    let shared = Mutex::new(fixture.processor_with(config));
    let mut harness = Harness::new();
    let (mut merge, _tx) = merge_of(events.into_iter().map(Script::of).collect());
    let mut out = Recorder::default();
    let _ = harness.pump.step(&shared, &mut merge, &mut out).unwrap();
    out.keys()
}

#[test]
fn a_layer_held_on_one_member_changes_the_keys_of_the_other() {
    let config = r#"(
        remaps: { KC_CAPS: MO("nav") },
        layers: { "nav": (kind: Remap, remaps: { KC_H: KC_LEFT }) },
        game_mode: (remaps: {}),
    )"#;
    let now = SystemTime::now();
    let ago = |ms: u64| now - Duration::from_millis(ms);
    let a = vec![vec![key_at(Key::KEY_CAPSLOCK, 1, ago(500)), key_at(Key::KEY_CAPSLOCK, 0, ago(100))]];
    let b = vec![vec![key_at(Key::KEY_H, 1, ago(300)), key_at(Key::KEY_H, 0, ago(250))]];
    let got = keys_through("layer", config, vec![a, b]);
    assert_eq!(got, vec![(Key::KEY_LEFT.code(), 1), (Key::KEY_LEFT.code(), 0)]);
}

#[test]
fn a_one_shot_modifier_tapped_on_one_member_applies_to_the_next_key_of_the_other() {
    let config = r"(
        remaps: { KC_CAPS: OSM(KC_LSFT) },
        layers: {},
        game_mode: (remaps: {}),
    )";
    let now = SystemTime::now();
    let ago = |ms: u64| now - Duration::from_millis(ms);
    let a = vec![vec![key_at(Key::KEY_CAPSLOCK, 1, ago(500)), key_at(Key::KEY_CAPSLOCK, 0, ago(450))]];
    let b = vec![vec![key_at(Key::KEY_X, 1, ago(300)), key_at(Key::KEY_X, 0, ago(250))]];
    let got = keys_through("oneshot", config, vec![a, b]);
    let shift = Key::KEY_LEFTSHIFT.code();
    let x = Key::KEY_X.code();
    assert_eq!(got.first(), Some(&(shift, 1)), "{got:?}");
    assert!(got.contains(&(x, 1)), "{got:?}");
    let at = |wanted: (u16, i32)| got.iter().position(|&event| event == wanted).unwrap();
    assert!(at((shift, 1)) < at((x, 1)) && at((x, 1)) < at((shift, 0)), "{got:?}");
}

#[test]
fn a_mod_tap_held_on_one_member_acts_as_its_modifier_for_a_key_on_the_other() {
    let config = r"(
        remaps: { KC_F: MT(KC_F, KC_LSFT) },
        layers: {},
        game_mode: (remaps: {}),
    )";
    let now = SystemTime::now();
    let ago = |ms: u64| now - Duration::from_millis(ms);
    // F has been down for a second (well past the tapping term) when J goes down elsewhere
    let a = vec![vec![key_at(Key::KEY_F, 1, ago(1000)), key_at(Key::KEY_F, 0, ago(100))]];
    let b = vec![vec![key_at(Key::KEY_J, 1, ago(500)), key_at(Key::KEY_J, 0, ago(450))]];
    let got = keys_through("modtap", config, vec![a, b]);
    let shift = Key::KEY_LEFTSHIFT.code();
    let j = Key::KEY_J.code();
    let at = |wanted: (u16, i32)| got.iter().position(|&event| event == wanted).unwrap();
    assert!(at((shift, 1)) < at((j, 1)), "{got:?}");
    assert!(!got.contains(&(Key::KEY_F.code(), 1)), "held, not tapped: {got:?}");
}
