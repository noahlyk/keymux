//! `keymux overlay --demo`: feeds the overlay made-up strokes, so the drawing can be
//! checked without the daemon, a steno layer, or a keyboard.

use crate::steno::layout::parse_stroke;
use crate::steno::overlay::{LookupRow, LookupView, OverlaySnapshot, TapeEntry, TAPE_CAPACITY};
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use std::time::Duration;

const STROKES: [(&str, &str); 12] = [
    ("TH", "\"this\""),
    ("S", "\"is\""),
    ("A", "\"a\""),
    ("TEFT", "\"test\""),
    ("OF", "\"of\""),
    ("-T", "\"the\""),
    ("STEPB", "\"steno\""),
    ("OEFR", "\"over\""),
    ("STKPWHR", ""),
    ("PWEUG", "\"big\""),
    ("TKPWAEUPL", "\"game\""),
    ("KWRAOU", "\"you\""),
];

/// Show the overlay, then press and release each demo stroke key by key, forever.
pub fn spawn(frames: Sender<OverlaySnapshot>) {
    std::thread::spawn(move || {
        let mut snapshot = OverlaySnapshot {
            layer: Some("demo".into()),
            ..OverlaySnapshot::default()
        };
        // Start with a full tape, so the whole-screen layout shows straight away
        let mut seq = 0;
        for (stroke, text) in STROKES.iter().cycle().take(TAPE_CAPACITY) {
            seq += 1;
            snapshot.tape.push_back(TapeEntry {
                seq,
                stroke: parse_stroke(stroke).expect("demo strokes are valid"),
                text: (*text).into(),
                unmatched: text.is_empty(),
                adopted: false,
            });
        }
        loop {
            for (stroke, text) in STROKES {
                let bits = parse_stroke(stroke).expect("demo strokes are valid");
                // Light the keys one at a time, then let go together
                let mut lit = 0;
                for slot in (0..23).filter(|slot| bits & (1 << slot) != 0) {
                    lit |= 1 << slot;
                    snapshot.generation += 1;
                    snapshot.chord = lit;
                    snapshot.held = lit;
                    if frames.send(snapshot.clone()).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(90));
                }
                seq += 1;
                if snapshot.tape.len() == TAPE_CAPACITY {
                    snapshot.tape.pop_front();
                }
                snapshot.tape.push_back(TapeEntry {
                    seq,
                    stroke: bits,
                    text: text.into(),
                    unmatched: text.is_empty(),
                adopted: false,
                });
                snapshot.generation += 1;
                snapshot.chord = 0;
                snapshot.held = 0;
                if frames.send(snapshot.clone()).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            if !lookup(&frames, &mut snapshot) {
                return;
            }
        }
    });
}

/// Words the demo lookup "finds": text and its outline.
const LOOKUP_WORDS: [(&str, &str); 5] = [
    ("candy", "KAPBD"),
    ("candle", "KAPBL"),
    ("candid", "KAPBD/EUD"),
    ("candidate", "KAPBD/TKAEUT"),
    ("cane", "KAEUPB"),
];

/// Open a lookup, type a word into it one letter at a time, then close it. Returns `false`
/// when the overlay has gone.
fn lookup(frames: &Sender<OverlaySnapshot>, snapshot: &mut OverlaySnapshot) -> bool {
    let show = |snapshot: &mut OverlaySnapshot, view: Option<LookupView>| {
        snapshot.generation += 1;
        snapshot.lookup = view;
        frames.send(snapshot.clone()).is_ok()
    };
    let word = "candy";
    for end in 0..=word.len() {
        let query = &word[..end];
        let rows: Vec<LookupRow> = LOOKUP_WORDS
            .iter()
            .filter(|(text, _)| end > 0 && text.starts_with(query))
            .map(|(text, outline)| LookupRow {
                text: (*text).into(),
                outline: (*outline).into(),
            })
            .collect();
        let hint = rows
            .first()
            .and_then(|row| parse_stroke(row.outline.split('/').next()?))
            .unwrap_or(0);
        let view = LookupView {
            query: query.into(),
            rows,
            hint,
        };
        if !show(snapshot, Some(view)) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(700));
    }
    std::thread::sleep(Duration::from_millis(1500));
    show(snapshot, None)
}
