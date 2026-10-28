# Steno overlay: plan

An on-screen steno keyboard and tape, drawn on the right edge of the screen while a
steno layer is active. Transparent, never takes clicks or focus, gone when steno is off.

Status: built on the `steno-overlay` branch. The tests pass and `keymux overlay --demo` has
been run on niri (surface accepted, translucent, hot reload works). **Not yet run against the
installed daemon**: that needs `make install` and a daemon restart. See **Steps**.

![preview](overlay-preview.png)

*Rendered offscreen by `OVERLAY_PREVIEW=/tmp/o.ppm cargo test preview -- --ignored` at 2x:
`T P H E` chord, `P H` still held.*

## Shape

```
 root daemon (evdev, no Wayland access)          user session
 ┌───────────────────────────────┐               ┌──────────────────────────────┐
 │ KeymapProcessor (per keyboard)│               │ keymux overlay               │
 │   steno press/release/stroke  │               │  ┌ ipc thread ┐  ┌ wayland ┐ │
 │        │ publish (non-block)  │   /run/       │  │ reads      │→ │ calloop │ │
 │        ▼                      │  keymux.sock  │  │ snapshots  │  │ redraw  │ │
 │   OverlayHub (Mutex+Condvar)  │──────────────▶│  └────────────┘  └─────────┘ │
 │   one streaming thread per    │  length-      │   layer-shell surface,       │
 │   subscriber                  │  prefixed     │   right edge, no input       │
 └───────────────────────────────┘  bincode      └──────────────────────────────┘
```

The daemon runs as root and can't reach the Wayland socket, so drawing happens in a
user process, the same split as the `niri-daemon` focus watcher. It ships as
`keymux overlay` with a `systemd/keymux-steno.service` user unit.

## 1. State in the daemon: `OverlayHub` — `src/steno/overlay.rs` [done, tested]

The overlay wants *the latest state*, not every event. So the hub stores one snapshot
and a generation counter instead of queueing events:

```rust
struct OverlaySnapshot {
    generation: u64,           // bumped on every change
    layer: Option<String>,     // Some(name) while a steno layer is on top → overlay visible
    chord: u32,                // stroke bits pressed so far in the stroke in progress
    held: u32,                 // stroke bits physically down right now (subset of chord)
    tape: VecDeque<TapeEntry>, // bounded ring, newest last, TAPE_CAPACITY entries
}
struct TapeEntry { seq: u64, stroke: u32, text: String, unmatched: bool }
```

- **Writers** (key path): lock, mutate, `generation += 1`, `notify_all`. The lock is
  uncontended (subscriber threads hold it only to clone ~1–2 KB), so a publish costs
  tens of nanoseconds. Never blocks on I/O. Never allocates for `chord`/`held`/`layer`
  changes. A tape push allocates one short `String`.
- **Readers** (one thread per subscriber): wait on the condvar until `generation`
  differs from the last one sent, clone, release the lock, serialize, write. A slow
  overlay just skips intermediate generations. It can never back-pressure typing.
- Coalescing is automatic: ten key events between two wakeups = one frame.
- The hub is a process-global (`OnceLock`), so `KeymapProcessor::new` and the many tests
  that build one don't need a new parameter. With no subscriber it still keeps state,
  so a newly started overlay gets the full current picture on its first frame.

Bits are Plover slot order (`steno::layout::SLOTS`), so the overlay needs no dictionary
or layout knowledge, only the 23-slot geometry.

### Publish points — `steno/engine.rs`, `event_processor/keymap.rs` [done, tested]

| Event                                   | Call                               |
|-----------------------------------------|------------------------------------|
| stroke key down / up                    | `hub.set_keys(chord, held)`        |
| stroke finished, translated             | `hub.push_stroke(stroke, text, unmatched)` |
| top layer changes (checked after every `process_key`) | `hub.set_layer(Some/None)` |
| `*` undo / Space retype                 | `push_stroke` with the retyped text |

`StenoEngine` needs two small getters: `chord_bits()` / `held_bits()` (from
`ChordState`), and `translate()` hands back `(stroke, description, unmatched)`, the same
values it already writes to the tape file, so the on-screen tape and
`keymux steno tape` always agree.

Layer detection today only happens on the next key press (`steno_active`). For the
overlay, compare `layer_stack.current_layer()` before/after each `process_key` and
publish on change. This makes `TO("steno")` show the overlay on the press itself.

Multiple keyboards: one hub shared by every keyboard, and every key event (press or
release) publishes *that keyboard's* steno layer, which the hub ignores when it is
unchanged. So the overlay follows whichever keyboard you touched last, re-evaluated on every
key: a key on a steno keyboard shows it, a key on one that is on base hides it. With a
steno-only Glove80 and a laptop keyboard, typing on the laptop hides the overlay and
touching the Glove80 brings it back. Stroke keys and the tape are shared state, so a stroke
mid-way when you switch keyboards is dropped from the display (the layer change clears it).

At daemon start a keyboard on a steno `default_layer` shows the overlay straight away. One
starting on base publishes nothing, so it can't hide the overlay for a steno keyboard that
started first; it takes over on its first key.

## 2. Wire protocol — `src/ipc.rs` [done, tested over a socket pair]

- New `IpcRequest::SubscribeOverlay`. The IPC accept loop handles it **before** the main
  loop: it hands the `UnixStream` to a new thread and goes back to accepting. The
  daemon's main loop never sees it.
- That thread writes `OverlayFrame` messages: `u32 LE length + bincode`, same framing as
  responses. It exits on the first write error (overlay closed).
- `OverlayFrame = OverlaySnapshot` for now. The tape holds up to 160 lines (enough for a
  tall screen), so a full frame is a few KB: still cheap to serialize on every key event.
  If that ever matters, send only `tape_since(seq)` instead.

## 3. Overlay process — `src/overlay/` [done; checked live on niri with `--demo`]

| File          | Job |
|---------------|-----|
| `mod.rs`      | `run()`: load config, start subscriber, run the Wayland loop |
| `client.rs`   | subscriber thread: connect, read frames, forward to the loop via a `calloop::channel`. Reconnects with backoff when the daemon restarts |
| `wayland.rs`  | sctk: one `LayerSurface` on `Layer::Overlay`, anchored `TOP \| BOTTOM \| RIGHT` with height 0 (the compositor makes it the output's height), `KeyboardInteractivity::None`, `exclusive_zone(-1)` (ignores bars, pushes nothing), empty input region (clicks fall through). Created when a steno layer turns on, destroyed when it turns off. The layout is rebuilt from the height in each configure |
| `render.rs`   | pixels with `tiny-skia`: the tape down the screen and the keyboard at the bottom, no background by default. Pure function of `(snapshot, config, scale, height)`, unit-testable without Wayland |
| `geometry.rs` | Steno key rectangles, computed once per size: 23 slots as tall pads (two rows per hand, `#` bar on top, `A O` / `E U` thumbs) |
| `glyphs.rs`   | `ab_glyph` rasterizer with a coverage cache: `[Option<Glyph>; 128]` for ASCII (array index, no hashing), `FxHashMap<char, Glyph>` for the rest. Each glyph is rasterized once per size |

### Font

Atkinson Hyperlegible, the font the steno-lessons site uses for keys (`--key-font`),
designed by the Braille Institute so that easily confused letters stay distinct.
Regular + Bold are embedded with `include_bytes!` from `assets/fonts/` (≈110 KB, SIL OFL
1.1, `assets/fonts/OFL.txt` ships beside them). Embedded means no fontconfig lookup and
the same look on every machine. `font: Some(path)` in config overrides it.

It's proportional, so the tape doesn't rely on monospace: every slot has its own column,
sized to its letter, and the letter is centred in it. Steno lines still align.

### Drawing cost

- **No timers.** The loop sleeps in `calloop` until a frame or a Wayland event arrives.
  Zero CPU while idle, zero while steno is off (surface unmapped).
- **Redraw only on a change.** A frame identical on screen (same layer, keys and newest
  tape line) is skipped. Layer changes create or destroy the surface.
- **Whole-surface redraw, no partial damage.** Measured in a release build with the demo
  redrawing several times a second on a full-height (340 × 1080) overlay for 21 s: 100 ms of
  CPU in total (≈0.5% of a core), 10 MB RSS. That includes startup and the soft text shadow, and
  works out to roughly a millisecond per frame, so tracking dirty rectangles would add code
  for nothing measurable. Revisit only if frames get much more expensive (a 4K screen is ~4x
  the pixels).
- **Frame callbacks**: at most one pending frame. A burst of strokes between vblanks is
  drawn once with the latest state.
- **Buffers**: sctk `SlotPool` (shm). It hands back a slot the compositor has released.
- **Pixel format**: `wl_shm::Format::Abgr8888` = tiny-skia's RGBA premultiplied byte order
  on little-endian, so tiny-skia draws straight into the shm buffer. Falls back to
  `Argb8888` with an R/B swap if a compositor doesn't offer it.
- Size: `width` (340) × the output's height, logical px; drawn at the output's integer scale.

### Look (from the mockups)

- No background by default (`background: 0.0`): only the keys and the text are drawn, so
  everything must read over any wallpaper. Light text gets a soft shadow: the glyph blurred
  (one pass of a 3 × 3 box, so it reaches one pixel; computed once per glyph and cached with
  it), drawn once underneath. Earlier versions stamped copies of the glyph at whole-pixel
  offsets, which looked blocky ("Minecraft text"), and then used a wider blur, which made
  small text look fuzzy. Dark text on a lit key gets no shadow (the key is its own
  background, and a shadow would only soften it); a test covers this.
- Keys are steno pads, 1.75× as tall as wide: a squarish top with slightly softened corners
  (radius 0.2× the width) and a fully round bottom. `S-` and `*` are double height, `#` is a
  thin pill across the top, thumbs sit below. Unlit keys are a faint glass fill with a thin
  light outline. Keys in `chord` but no longer in `held` are dimmer, so you see the stroke
  building.
- Lit keys are coloured by part of the keyboard (`KeyGroup` in `geometry.rs`): left hand
  green, thumb vowels yellow, right hand a soft red, and `*` and `#` light blue.
- The overlay is as tall as the screen, pinned to the right edge. The keyboard sits at the
  bottom; the tape fills everything above it, newest line at the bottom. How many lines fit
  is worked out from the height the compositor configures.
- Tape lines are Atkinson Hyperlegible Bold throughout. The strokes are the steno letters in
  soft light grey (not green: it fought the wallpaper), each slot in its own column, so
  strokes line up from line to line, as on a paper tape. The column widths are not a uniform
  grid: each is as wide as its own letter plus a 4% gap (`SLOT_GAP`), so `W` gets more room
  than `T`, nothing overlaps, and neighbouring letters sit as tight as ordinary text. That
  spacing applies to the steno letters only; the words are set with normal spacing. The words
  (what the stroke typed) are near-white and at least as big as the strokes (1.06×), since
  that is what you read. Unmatched strokes show a red `?`.
- Every tape line is full strength. Only the top three lines fade (25%, 50%, 75%), so the tape
  stays easy to read all the way up.
- All sizes come from `width`: the steno letters are sized so the 23 columns (each its
  letter plus the 4% gap) fill 68% of the width inside the padding, which at the default
  width of 340 makes them about 11 px tall (they were 9 px when they filled less and had
  looser columns, and read as too small); key labels are 56% of a key's width. `text_scale`
  multiplies the text on top. Widths of 400 and up were tried and read as too big.
- **Sharpness.** The text is not hinted, so it is made sharp by construction:
  - Every size is snapped (`glyphs::crisp_size`) so the height of `H` (capitals) or `x`
    (lowercase words) is a whole number of pixels. Otherwise flat tops and bottoms (`E`, `T`,
    `H`) straddle two pixel rows and smear into soft grey lines. A test checks the edges are
    solid rows at the snapped size and smeared at an arbitrary one (12.6 px).
  - Every letter is placed on a whole pixel, and words advance by whole pixels, so vertical
    stems sit on one pixel column. The cost is letter gaps that are off by up to half a
    pixel. (Quarter-pixel placement was tried first, for even spacing, and looked soft.)

## 4. Config — `src/config/config.rs` [done, hot reload checked live]

```ron
overlay: (
    enabled: true,
    output: None,          // Some("DP-1") to pin a monitor; None = compositor's choice
    margin: 0,             // gap from the right screen edge, logical px
    width: 340,            // as tall as the screen; text and keys are sized from this
    text_scale: 1.0,       // multiplies the text size on top of what width gives
    background: 0.0,       // dark panel behind it all; 0.0 is fully transparent
    keyboard: true,
    tape: true,
    tape_lines: None,      // Some(20) caps it; None fills the screen height
    font: None,            // path to a .ttf/.otf; None = embedded Atkinson Hyperlegible
    layers: [],            // steno layers that show it; empty = every steno layer
),
```

The overlay process reads this itself (the user can read their own config). The daemon
doesn't need it. Visibility rules stay in the overlay, so changing look/size never
touches the daemon. Hot reload: a thread watches the config directory with `notify`,
reparses on a save, and the overlay rebuilds its layout and recreates the surface.

## 5. Steps

Done and tested: hub, publish points (`steno_tests.rs` drives a real `KeymapProcessor` into a
private hub), IPC stream (socket-pair test), drawing, config and hot reload, systemd unit,
README, example config.

Done by hand on niri with `keymux overlay --demo`: surface accepted on the Overlay layer with
keyboard interactivity none; panel translucent over windows; config edits applied live;
surface gone when the process stops.

**Still to do, needs your machine:**
1. `make install`, restart the daemon, run `keymux overlay --dump`, write some strokes.
   Proves the daemon-to-overlay path with real keyboard input.
2. `keymux overlay`, then switch into steno: the panel should appear on that key press.
3. Click on the panel's area: the click should reach the window underneath. Not tested yet.
4. `systemctl --user enable --now keymux-steno.service`.

## 6. Live lookup — `src/steno/lookup.rs` [done, tested; not yet tried on real hardware]

"Midway through typing, forgot the stroke for a word." A `{PLOVER:LOOKUP}` dictionary entry opens a
submode **inside the steno engine**: the layer doesn't change, but while it is open the layer's
keys are plain QWERTY input to a query buffer, all swallowed so nothing reaches the app.

- **Trigger.** An ordinary dictionary entry, as in Plover (`run_command` handles `lookup`).
  There is no config setting for it any more.
- **Keys.** `keymap.rs` routes every key to the open lookup (`lookup_key`), US QWERTY by
  keycap, Shift capitalizes (the dictionary is case-sensitive), Backspace edits, Esc/Enter
  close. Esc and Enter win over the layer's own remaps (a steno layer usually remaps Esc);
  other remapped keys, such as one that leaves steno, still work and leave the lookup behind.
  Shift is tracked in `process_key` itself, because its release is swallowed.
- **Results.** Exact outlines for the word first (fewest strokes first, up to 4, only ones that
  type it "properly" in a sentence), then words that start with the query, shortest first, each
  with its best outline (up to 12 rows; completions only from 2 characters). Built in the daemon
  and sent in the snapshot as `LookupView { query, rows, hint }`, so the overlay needs no
  dictionary.
- **Cost.** Per key press: one hash lookup plus a scan of the distinct translations for the
  completions. `proper_outlines_in` works from the engine's translator, whose dictionary is
  shared through an `Arc`; the existing `proper_outlines` copied the whole dictionary per call,
  which is fine for a CLI command and not for every key press.
- **Overlay.** The lookup replaces the tape, bottom-up like it: a `LOOKUP` label and the query
  with a caret sit just above the keyboard, the results above them with the best nearest, and
  the first stroke of the best result is lit dimly on the keyboard. The "has anything changed"
  check in `wayland.rs` includes the lookup, or typing wouldn't redraw.
- **Closing.** Esc/Enter, or leaving the steno layer (the hub clears it on a layer change, and
  the engine on re-entry), so a lookup can never be left eating keys.
- **Needs the overlay.** The query and results are only drawn there, so without
  `keymux overlay` running you'd be typing blind. A terminal fallback is `keymux steno keys`.

## Risks / open questions

- Clicks falling through the overlay: the empty input region is set, and niri reports
  keyboard interactivity none, but a real click hasn't been tried.
- It covers the whole right edge, including the top-right corner of a bar (DMS's bar items
  there, such as a power button). With no background only the faded top tape lines overlap
  it, and clicks pass through, but they are drawn over it. `margin` or `tape_lines` moves it.
- Fullscreen windows: `Layer::Overlay` sits above them. Maybe hide in game mode
  (the hub already knows `disabled_in_game_mode` layers aren't capturing).
- Fractional scale: integer scales are handled (the panel redraws at the output's scale).
  A fractional scale gets the next integer up, which the compositor shrinks. Add
  `wp_fractional_scale` + viewporter only if it looks soft. All three of your monitors are
  at scale 1, so this is untested on a scaled output.
- GNOME has no layer-shell: the overlay logs and exits there. Not a target.
