//! `keymux overlay --demo`: feeds the overlay made-up strokes, so the drawing can be
//! checked without the daemon, a steno layer, or a keyboard.

use crate::steno::layout::{parse_stroke, parse_strokes_trimmed};
use crate::steno::overlay::{
    LookupMode, LookupRow, LookupView, OverlayHub, OverlaySnapshot, TAPE_CAPACITY,
};
use smithay_client_toolkit::reexports::calloop::channel::Sender;
use std::time::Duration;

// An undo and a miss taken back show what a later stroke took back, grey
// (a few use the groups that fuse into one key: T P H, P H, S K W R, -P B, -G S, ...)
const STROKES: [(&str, &str); 19] = [
    ("TH", "\"this\""),
    ("S", "\" is\""),
    ("A", "\" a\""),
    ("TEFT", "\" test\""),
    ("OF", "\" of\""),
    ("-T", "\" the\""),
    ("STEPB", "\" stone\""),
    ("*", "-6 \"\""),
    ("STEPB", "\" steno\""),
    ("OEFR", "\" over\""),
    ("STKPWHR", "? \" STKPWHR\""),
    ("*", "-8 \"\""),
    ("PWEUG", "\" big\""),
    ("TKPWAEUPL", "\" game\""),
    ("KWRAOU", "\" you\""),
    ("TPHOE", "\" no\""),
    ("PHAEUPB", "\" main\""),
    ("SKWRUPL", "\" jump\""),
    ("TKPWAO-GS", "\" good\""),
];

/// Show the overlay, then press and release each demo stroke key by key, forever.
pub fn spawn(frames: Sender<OverlaySnapshot>) {
    std::thread::spawn(move || {
        // A hub of its own, so the demo's tape is kept (and greyed) as the daemon's is
        let hub = OverlayHub::default();
        hub.set_layer(Some("demo"));
        let push = |stroke: &str, text: &str| {
            let bits = parse_stroke(stroke).expect("demo strokes are valid");
            hub.push_stroke(bits, text.into(), text.starts_with('?'));
        };
        // Start with a full tape, so the whole-screen layout shows straight away
        for (stroke, text) in STROKES.iter().cycle().take(TAPE_CAPACITY) {
            push(stroke, text);
        }
        loop {
            for (stroke, text) in STROKES {
                let bits = parse_stroke(stroke).expect("demo strokes are valid");
                // Light the keys one at a time, then let go together
                let mut lit = 0;
                for slot in (0..23).filter(|slot| bits & (1 << slot) != 0) {
                    lit |= 1 << slot;
                    hub.set_keys(lit, lit);
                    if frames.send(hub.snapshot()).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(90));
                }
                push(stroke, text);
                if frames.send(hub.snapshot()).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
            if !lookup(&frames, &hub) {
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
fn lookup(frames: &Sender<OverlaySnapshot>, hub: &OverlayHub) -> bool {
    let show = |view: Option<LookupView>| {
        hub.set_lookup(view);
        frames.send(hub.snapshot()).is_ok()
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
        let strokes = rows
            .first()
            .and_then(|row| parse_strokes_trimmed(&row.outline))
            .unwrap_or_default();
        let view = LookupView {
            query: query.into(),
            rows,
            hint,
            strokes,
            mode: LookupMode::Word,
        };
        if !show(Some(view)) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(700));
    }
    std::thread::sleep(Duration::from_millis(1500));
    show(None)
}
