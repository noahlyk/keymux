//! Times the dictionary operations a key press can trigger, over a real folder of dictionaries:
//! `cargo run --release --example perf_probe -- DIR CACHE`.
use keymux::steno::dict::Dictionary;
use keymux::steno::layout::Layout;
use keymux::steno::reverse::nearby_strokes;
use std::path::PathBuf;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(args.next().expect("DIR"));
    let cache = PathBuf::from(args.next().expect("CACHE"));
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    let layout = Layout::plover_with(&['+']).unwrap();
    let time = |what: &str, f: &mut dyn FnMut() -> usize| {
        let t = Instant::now();
        let n = f();
        println!("{what:34} {:8.1} ms  ({n})", t.elapsed().as_secs_f64() * 1e3);
    };
    let mut dict = Dictionary::default();
    time("load_cached", &mut || {
        dict = Dictionary::load_cached(&files, &cache, &layout);
        0
    });
    time("len (entries)", &mut || dict.len());
    let kphraoet = layout.parse_strokes("KPHRAOET").unwrap();
    time("lookup exact", &mut || usize::from(dict.lookup(&kphraoet).is_some()));
    time("strokes_for(complete)", &mut || dict.strokes_for("complete").len());
    time("strokes_for(compromise)", &mut || dict.strokes_for("compromise").len());
    time("completions(com, 24)", &mut || dict.completions("com", 24).len());
    time("completions(comp, 24)", &mut || dict.completions("comp", 24).len());
    time("nearby_strokes(1 stroke)", &mut || nearby_strokes(&dict, &kphraoet, 8).len());
    let two = layout.parse_strokes("KOPL/PRO").unwrap();
    time("nearby_strokes(2 strokes)", &mut || nearby_strokes(&dict, &two, 8).len());
    time("iter().count()", &mut || dict.iter().count());
}
