//! The output side, end to end: what the keymap decides leaves the virtual keyboard as exactly
//! those events, paced, and a pass of the loop gives each key to the keymap at the moment the
//! keyboard reported it. The `uinput` test goes through a real virtual device.

use super::control::{self, Control, Doorbell, Signal};
use super::output::{self, Pacer, Sink, SAFE_EVENTS, UNREAD_WINDOW};
use super::pump::{Pump, Source, Step};
use super::{KeymapProcessor, ProcessResult};
use crate::config::Config;
use crate::steno::translate::OutputOp;
use evdev::{EventType, InputEvent, Key};
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

/// Records everything emitted, as (type, code, value).
#[derive(Default)]
struct Recorder {
    events: Vec<(EventType, u16, i32)>,
}

impl Sink for Recorder {
    fn emit(&mut self, events: &[InputEvent]) -> io::Result<()> {
        self.events
            .extend(events.iter().map(|e| (e.event_type(), e.code(), e.value())));
        Ok(())
    }
}

impl Recorder {
    /// The key events, as (code, value).
    fn keys(&self) -> Vec<(u16, i32)> {
        self.events
            .iter()
            .filter(|(kind, _, _)| *kind == EventType::KEY)
            .map(|&(_, code, value)| (code, value))
            .collect()
    }

    /// The text the key presses would type, letters and spaces only.
    fn typed(&self) -> String {
        self.keys()
            .iter()
            .filter(|&&(_, value)| value == 1)
            .filter_map(|&(code, _)| {
                let name = format!("{:?}", Key::new(code));
                match name.strip_prefix("KEY_")? {
                    "SPACE" => Some(' '),
                    letter if letter.len() == 1 => letter.chars().next()?.to_lowercase().next(),
                    _ => None,
                }
            })
            .collect()
    }
}

/// A key press and release, each followed by a SYN_REPORT.
fn tap(key: Key) -> Vec<(EventType, u16, i32)> {
    vec![
        (EventType::KEY, key.code(), 1),
        (EventType::SYNCHRONIZATION, 0, 0),
        (EventType::KEY, key.code(), 0),
        (EventType::SYNCHRONIZATION, 0, 0),
    ]
}

fn emitted(result: ProcessResult) -> Vec<(EventType, u16, i32)> {
    let mut out = Recorder::default();
    let mut pacer = Pacer::default();
    output::emit_result(&mut out, &mut pacer, result).unwrap();
    out.events
}

#[test]
fn a_retype_deletes_then_types_each_key_with_a_syn_report() {
    let events = emitted(ProcessResult::Retype {
        backspaces: 7,
        text: "course".into(),
    });
    let mut expected = Vec::new();
    for _ in 0..7 {
        expected.extend(tap(Key::KEY_BACKSPACE));
    }
    for key in [
        Key::KEY_C,
        Key::KEY_O,
        Key::KEY_U,
        Key::KEY_R,
        Key::KEY_S,
        Key::KEY_E,
    ] {
        expected.extend(tap(key));
    }
    assert_eq!(events, expected);
}

#[test]
fn ops_with_backspaces_then_text_emit_the_same_as_a_retype() {
    let ops = emitted(ProcessResult::Ops(vec![
        OutputOp::Backspaces(7),
        OutputOp::Text("course".into()),
    ]));
    let retype = emitted(ProcessResult::Retype {
        backspaces: 7,
        text: "course".into(),
    });
    assert_eq!(ops, retype);
}

#[test]
fn a_shifted_character_holds_shift_around_its_own_keystroke_only() {
    let events = emitted(ProcessResult::TypeString("aB".into()));
    let mut expected = tap(Key::KEY_A);
    expected.push((EventType::KEY, Key::KEY_LEFTSHIFT.code(), 1));
    expected.push((EventType::SYNCHRONIZATION, 0, 0));
    expected.extend(tap(Key::KEY_B));
    expected.push((EventType::KEY, Key::KEY_LEFTSHIFT.code(), 0));
    expected.push((EventType::SYNCHRONIZATION, 0, 0));
    assert_eq!(events, expected);
}

#[test]
fn the_pacer_keeps_in_flight_events_under_the_kernel_queue_and_never_splits_a_keystroke() {
    let t0 = Instant::now();
    // Shifted keystrokes (8 events) among plain ones (4), more than one window's worth
    let sizes: Vec<usize> = (0..30).map(|i| if i % 3 == 0 { 8 } else { 4 }).collect();
    let parts = Pacer::default().schedule(t0, &sizes);

    // Every keystroke goes out exactly once, whole, in order, never earlier than the one before
    let mut next = 0;
    let mut last_at = t0;
    for (at, range) in &parts {
        assert_eq!(range.start, next, "{parts:?}");
        assert!(range.end > range.start);
        assert!(*at >= last_at, "{parts:?}");
        next = range.end;
        last_at = *at;
    }
    assert_eq!(next, sizes.len());

    // Within any one unread window, no more than the kernel's queue can hold
    for (at, _) in &parts {
        let in_window: usize = parts
            .iter()
            .filter(|(other, _)| other >= at && *other < *at + UNREAD_WINDOW)
            .map(|(_, range)| sizes[range.clone()].iter().sum::<usize>())
            .sum();
        assert!(
            in_window <= SAFE_EVENTS,
            "{in_window} events within one window"
        );
    }
    assert!(
        parts.len() > 1,
        "{} events should not all fit at once",
        sizes.iter().sum::<usize>()
    );

    // A keystroke bigger than the whole budget still goes, on its own
    let parts = Pacer::default().schedule(t0, &[SAFE_EVENTS + 1, 4]);
    assert_eq!(parts[0].1, 0..1);
    assert_eq!(parts[1].1, 1..2);
}

/// A keyboard that starts on a steno layer whose dictionary knows one word, `KAT` = cat, in a
/// temp directory of its own.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("keymux-emission-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn processor(&self) -> KeymapProcessor {
        let dict = self.dir.join("main.json");
        std::fs::write(&dict, r#"{"KAT": "cat"}"#).unwrap();
        let config_path = self.dir.join("config.ron");
        let config_text = format!(
            r#"(
    default_layer: Some("chords"),
    remaps: {{}},
    layers: {{
        "chords": (
            kind: Steno((dictionaries: ["{}"])),
            remaps: {{}},
            disabled_in_game_mode: true,
        ),
    }},
    game_mode: (remaps: {{}}),
)"#,
            dict.display()
        );
        std::fs::write(&config_path, config_text).unwrap();
        let config = Config::load(&config_path).unwrap();
        let mut keymap = KeymapProcessor::new(&config, config_path, 0);
        keymap.wait_for_steno_dictionaries();
        keymap
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A keyboard whose events are scripted: each `read` hands over the next batch, and with none
/// left there is nothing to read.
#[derive(Default)]
struct Script {
    batches: VecDeque<Vec<InputEvent>>,
    waits: Vec<Option<Instant>>,
}

impl Script {
    fn of(batches: Vec<Vec<InputEvent>>) -> Self {
        Self {
            batches: batches.into(),
            waits: Vec::new(),
        }
    }
}

impl Source for Script {
    fn read(&mut self, into: &mut Vec<InputEvent>) -> io::Result<()> {
        self.batches.pop_front().map_or_else(
            || Err(io::ErrorKind::WouldBlock.into()),
            |batch| {
                into.extend(batch);
                Ok(())
            },
        )
    }

    fn wait(&mut self, deadline: Option<Instant>, _bell: &Doorbell) {
        self.waits.push(deadline);
    }
}

/// A key event as the kernel would report it, stamped `at`.
fn key_at(key: Key, value: i32, at: SystemTime) -> InputEvent {
    let since = at.duration_since(SystemTime::UNIX_EPOCH).unwrap();
    InputEvent::from(libc::input_event {
        time: libc::timeval {
            tv_sec: i64::try_from(since.as_secs()).unwrap(),
            tv_usec: i64::from(since.subsec_micros()),
        },
        type_: EventType::KEY.0,
        code: key.code(),
        value,
    })
}

/// A pump with the daemon's end of its line, which must outlive it: dropping that end reads
/// as a shutdown.
struct Harness {
    pump: Pump,
    control: Control,
}

impl Harness {
    fn new() -> Self {
        let (control, controlled) = control::channel().unwrap();
        Self {
            pump: Pump::new("test keyboard".into(), 0, controlled),
            control,
        }
    }
}

const KAT: [Key; 3] = [Key::KEY_S, Key::KEY_C, Key::KEY_P];

/// `KAT` tapped, then pressed again right after and held, all reported at `stamp(ms ago)`.
fn tapped_then_held(stamp: impl Fn(u64) -> SystemTime) -> Vec<InputEvent> {
    let mut batch = Vec::new();
    for key in KAT {
        batch.push(key_at(key, 1, stamp(400)));
    }
    for key in KAT {
        batch.push(key_at(key, 0, stamp(350)));
    }
    for key in KAT {
        batch.push(key_at(key, 1, stamp(300)));
    }
    batch
}

#[test]
fn a_batch_that_waited_is_replayed_at_the_moments_the_keyboard_reported() {
    let fixture = Fixture::new("stamps");
    let shared = Mutex::new(fixture.processor());
    let mut harness = Harness::new();
    let now = SystemTime::now();
    let mut source = Script::of(vec![tapped_then_held(|ms| {
        now.checked_sub(Duration::from_millis(ms)).unwrap()
    })]);
    let mut out = Recorder::default();

    assert_eq!(
        harness.pump.step(&shared, &mut source, &mut out).unwrap(),
        Step::Read
    );
    // The tap typed the word. By the keyboard's clock the second press is 300ms old - the
    // repeat delay - so the idle pass that follows repeats it at once, without any waiting
    assert_eq!(
        harness.pump.step(&shared, &mut source, &mut out).unwrap(),
        Step::Idle
    );
    assert_eq!(
        out.typed().split_whitespace().collect::<Vec<_>>(),
        ["cat", "cat"]
    );
    // And the wait that followed was for the next repeat, not the idle default
    assert!(
        matches!(source.waits.as_slice(), [Some(_)]),
        "{:?}",
        source.waits
    );
}

#[test]
fn the_same_batch_reported_just_now_is_a_held_stroke_that_has_not_repeated_yet() {
    let fixture = Fixture::new("fresh");
    let shared = Mutex::new(fixture.processor());
    let mut harness = Harness::new();
    let now = SystemTime::now();
    // The same events, but all reported this instant
    let mut source = Script::of(vec![tapped_then_held(|_| now)]);
    let mut out = Recorder::default();

    assert_eq!(
        harness.pump.step(&shared, &mut source, &mut out).unwrap(),
        Step::Read
    );
    assert_eq!(
        harness.pump.step(&shared, &mut source, &mut out).unwrap(),
        Step::Idle
    );
    assert_eq!(out.typed().split_whitespace().collect::<Vec<_>>(), ["cat"]);
}

#[test]
fn alt_f12_in_the_middle_of_a_batch_releases_everything_and_stops() {
    let fixture = Fixture::new("escape");
    let shared = Mutex::new(fixture.processor());
    let mut harness = Harness::new();
    let now = SystemTime::now();
    let batch = vec![
        key_at(Key::KEY_LEFTALT, 1, now),
        key_at(Key::KEY_F12, 1, now),
        key_at(Key::KEY_A, 1, now),
    ];
    let mut source = Script::of(vec![batch]);
    let mut out = Recorder::default();

    assert_eq!(
        harness.pump.step(&shared, &mut source, &mut out).unwrap(),
        Step::Escaped
    );
    let keys = out.keys();
    assert!(
        !keys.contains(&(Key::KEY_A.code(), 1)),
        "the key after the chord must not be typed: {keys:?}"
    );
    assert!(
        keys.contains(&(Key::KEY_LEFTALT.code(), 0)),
        "the modifiers are released on the way out: {keys:?}"
    );
}

#[test]
fn a_shutdown_signal_releases_everything_and_stops() {
    let fixture = Fixture::new("shutdown");
    let shared = Mutex::new(fixture.processor());
    let mut harness = Harness::new();
    let mut source = Script::default();
    let mut out = Recorder::default();
    harness.control.send(Signal::Shutdown);

    assert_eq!(
        harness.pump.step(&shared, &mut source, &mut out).unwrap(),
        Step::Shutdown
    );
    assert!(out.keys().contains(&(Key::KEY_LEFTSHIFT.code(), 0)));
}

#[test]
fn events_that_are_not_keys_pass_through_unchanged() {
    let fixture = Fixture::new("passthrough");
    let shared = Mutex::new(fixture.processor());
    let mut harness = Harness::new();
    let syn = InputEvent::new(EventType::SYNCHRONIZATION, 0, 0);
    let mut source = Script::of(vec![vec![syn]]);
    let mut out = Recorder::default();

    assert_eq!(
        harness.pump.step(&shared, &mut source, &mut out).unwrap(),
        Step::Read
    );
    assert_eq!(out.events, vec![(EventType::SYNCHRONIZATION, 0, 0)]);
}

/// Through a real virtual keyboard: the device is grabbed by this test before anything is
/// sent, so the compositor never sees a key of it.
#[test]
#[ignore = "needs /dev/uinput: cargo test --lib uinput -- --ignored"]
fn uinput_round_trip_emits_exactly_the_backspaces_asked_for() {
    use evdev::uinput::VirtualDeviceBuilder;
    use evdev::{AttributeSet, Device};
    use std::os::unix::io::AsRawFd;

    let mut keys = AttributeSet::<Key>::new();
    for key in [
        Key::KEY_BACKSPACE,
        Key::KEY_LEFTSHIFT,
        Key::KEY_C,
        Key::KEY_O,
        Key::KEY_U,
        Key::KEY_R,
        Key::KEY_S,
        Key::KEY_E,
    ] {
        keys.insert(key);
    }
    let built = VirtualDeviceBuilder::new().and_then(|builder| {
        builder
            .name("keymux emission test")
            .with_keys(&keys)?
            .build()
    });
    let Ok(mut virtual_device) = built else {
        eprintln!("skipping: /dev/uinput is not available here");
        return;
    };

    // The node takes a moment to appear. Open it and grab it, so nothing else reads it
    let mut reader = None;
    for _ in 0..100 {
        if let Ok(nodes) = virtual_device.enumerate_dev_nodes_blocking() {
            for node in nodes.flatten() {
                if let Ok(mut device) = Device::open(&node) {
                    if device.grab().is_ok() {
                        reader = Some(device);
                        break;
                    }
                }
            }
        }
        if reader.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let Some(mut reader) = reader else {
        eprintln!("skipping: the virtual device's node could not be opened and grabbed");
        return;
    };
    let fd = reader.as_raw_fd();
    // SAFETY: fd is the open reader's own descriptor, and only its flags are touched
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }

    let mut pacer = Pacer::default();
    output::emit_result(
        &mut virtual_device,
        &mut pacer,
        ProcessResult::Retype {
            backspaces: 7,
            text: "course".into(),
        },
    )
    .unwrap();

    let mut presses = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while presses.len() < 13 && Instant::now() < deadline {
        match reader.fetch_events() {
            Ok(events) => presses.extend(
                events
                    .filter(|e| e.event_type() == EventType::KEY && e.value() == 1)
                    .map(|e| e.code()),
            ),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => panic!("reading the virtual device back: {e}"),
        }
    }
    let backspace = Key::KEY_BACKSPACE.code();
    assert_eq!(
        presses.iter().filter(|&&code| code == backspace).count(),
        7,
        "{presses:?}"
    );
    assert_eq!(presses.len(), 13, "{presses:?}");
    assert!(
        presses[..7].iter().all(|&code| code == backspace),
        "the deletes come before the text: {presses:?}"
    );
}
