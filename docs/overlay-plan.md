# Steno overlay: plan

An on-screen steno keyboard and tape, drawn on the right edge of the screen while a
steno layer is active. Transparent, never takes clicks or focus, gone when steno is off.

Status: skeleton on the `steno-overlay` branch. It builds, the tests pass, and every
piece below exists, but nothing has run against the real daemon or niri yet. Open work is
marked `TODO(overlay)` in the code and listed under **Steps**.

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
`keymux overlay` with a `systemd/keymux-overlay.service` user unit.

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

### Publish points — `steno/engine.rs`, `event_processor/keymap.rs` [wired, untested]

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

Multiple keyboards: one hub, last writer wins, so the overlay follows the keyboard you
last pressed a key on. Typing on the laptop board hides it; a key on the Glove80 brings it
back.

Known gap: with `default_layer: Some("steno")` the overlay appears on the first key
press, not at daemon start, because only key events publish the layer.

## 2. Wire protocol — `src/ipc.rs` [written, untested]

- New `IpcRequest::SubscribeOverlay`. The IPC accept loop handles it **before** the main
  loop: it hands the `UnixStream` to a new thread and goes back to accepting. The
  daemon's main loop never sees it.
- That thread writes `OverlayFrame` messages: `u32 LE length + bincode`, same framing as
  responses. It exits on the first write error (overlay closed).
- `OverlayFrame = OverlaySnapshot` for now. Small enough that diffs aren't worth it
  (≤ 24 tape lines × ~30 B). If the tape grows, send `tape_since(seq)` instead.

## 3. Overlay process — `src/overlay/` [written; render/geometry/glyphs tested offscreen]

| File          | Job |
|---------------|-----|
| `mod.rs`      | `run()`: load config, start subscriber, run the Wayland loop |
| `client.rs`   | subscriber thread: connect, read frames, forward to the loop via a `calloop::channel`. Reconnects with backoff when the daemon restarts |
| `wayland.rs`  | sctk: one `LayerSurface` on `Layer::Overlay`, anchored `RIGHT` (vertically centred), `KeyboardInteractivity::None`, `exclusive_zone(0)` (doesn't push windows), empty input region (clicks fall through). Mapped when `layer.is_some()`, unmapped (buffer detached) when not |
| `render.rs`   | `Scene` → pixels with `tiny-skia`: rounded translucent panel, the keyboard, the tape. Pure function of `(snapshot, theme, size)`, unit-testable without Wayland |
| `geometry.rs` | Steno key rectangles, computed once per size: 23 slots laid out like the mockup (two rows per hand, `#` bar on top, `A O` / `E U` thumbs) |
| `glyphs.rs`   | `ab_glyph` rasterizer with a coverage cache: `[Option<Glyph>; 128]` for ASCII (array index, no hashing), `FxHashMap<char, Glyph>` for the rest. Each glyph is rasterized once per size |

### Font

Atkinson Hyperlegible, the font the steno-lessons site uses for keys (`--key-font`),
designed by the Braille Institute so that easily confused letters stay distinct.
Regular + Bold are embedded with `include_bytes!` from `assets/fonts/` (≈110 KB, SIL OFL
1.1, `assets/fonts/OFL.txt` ships beside them). Embedded means no fontconfig lookup and
the same look on every machine. `font: Some(path)` in config overrides it.

It's proportional, so the tape doesn't rely on monospace: every slot has a fixed column
x-position and its letter is centred in that column. Steno lines still align.

### Drawing cost

- **No timers.** The loop sleeps in `calloop` until a frame or a Wayland event arrives.
  Zero CPU while idle, zero while steno is off (surface unmapped).
- **Redraw only on a new generation.** Compare the new snapshot with the drawn one:
  - keys changed only → repaint the keyboard rect, `damage_buffer` that rect.
  - tape changed → repaint the tape rect.
  - layer changed → map/unmap.
- **Frame callbacks**: at most one pending frame. A burst of strokes between vblanks is
  drawn once with the latest state.
- **Buffers**: sctk `SlotPool` (shm), two buffers, reuse when the compositor released.
- **Pixel format**: prefer `wl_shm::Format::Abgr8888` = tiny-skia's RGBA premultiplied
  byte order on little-endian, so tiny-skia draws straight into the shm buffer. Fall
  back to `Argb8888` with an in-place R/B swap of the damaged rect only.
- Panel size ≈ 280 × 700 logical px; scaled by the output's integer scale.

### Look (from the mockups)

- Panel: near-black, ~70% alpha, 14 px radius, no border.
- Keys: dark slate, rounded 6 px. Lit in the stroke accent (teal for consonants,
  pale yellow for vowels, as in the mockup). Keys in `chord` but no longer in `held`
  are drawn at reduced intensity, so you see the stroke building.
- Tape above the keyboard: Atkinson Hyperlegible, one stroke per line in steno order
  with gaps (`TP   H    E` style, every slot has a fixed column), newest at the bottom,
  older lines fade out. The translation sits to the right in the accent colour,
  unmatched strokes as `?` in red.

## 4. Config — `src/config/config.rs` [done; hot reload todo]

```ron
overlay: (
    enabled: true,
    output: None,          // Some("DP-1") to pin a monitor; None = compositor's choice
    margin: 16,            // gap from the screen edge, logical px
    width: 280,
    opacity: 0.72,         // panel background alpha
    keyboard: true,
    tape: true,
    tape_lines: 18,
    font: None,            // path to a .ttf/.otf; None = embedded Atkinson Hyperlegible
    layers: [],            // steno layers that show it; empty = every steno layer
),
```

The overlay process reads this itself (the user can read their own config). The daemon
doesn't need it. Visibility rules stay in the overlay, so changing look/size never
touches the daemon. Hot reload: watch the config file with `notify` (already a dep).

## 5. Steps

1. **Hub + tests**: done.
2. **IPC subscribe + stream thread**: written. Todo: a socket-pair test, and noticing a
   closed subscriber without the 30 s heartbeat write.
3. **Publish points**: wired. Todo: tests in `steno_tests.rs` asserting the hub
   (tests share the global hub, so give the hub a test hook or inject it).
4. **`keymux overlay --dump`**: written. Next step: `make install`, restart the daemon,
   run `keymux overlay --dump`, write a few strokes. Proves 1–3 on the real daemon.
5. **Wayland surface**: written. Check on niri: no focus steal, clicks pass through,
   above fullscreen, `layer-rule` namespace works.
6. **Render**: full redraw written. Todo: partial damage, two reused buffers, output
   scale, prettier retype/key-press text on the tape.
7. **Todo**: config hot reload, `systemd/keymux-overlay.service`, README section,
   `config.example.ron` entry.

## Risks / open questions

- niri and empty input regions: supported (smithay honours `set_input_region`), verify.
- Fullscreen windows: `Layer::Overlay` sits above them. Maybe hide in game mode
  (the hub already knows `disabled_in_game_mode` layers aren't capturing).
- Fractional scale: start with integer `wl_output` scale; add `wp_fractional_scale`
  + viewporter only if it looks soft on your monitor.
- GNOME has no layer-shell: the overlay logs and exits there. Not a target.
