# Keyboard Middleware - Architecture & Features

## Overview

This document describes the architecture and comprehensive feature set of the keyboard middleware system, designed to provide high-performance, fully customizable key processing with robust management capabilities.

This is a **QMK-inspired keyboard remapping daemon** that runs on Linux, providing instant key processing with zero-latency while supporting advanced features like home row mods, layers, SOCD, game mode detection, and command execution.

## Design Goals

1. **Zero-latency hot path**: Key event processing must be synchronous and allocation-free
2. **100% customizable**: Every key on every keyboard can be remapped, no hardcoded layouts
3. **Async management**: Device discovery, configuration, and user management should be async
4. **Multi-user support**: Proper session-based keyboard ownership
5. **Self-healing**: Automatic recovery from failures and clean error handling
6. **Hot-reload**: Configuration changes without service interruption

---

## Feature Set (QMK-Inspired)

### Core Features

#### 1. **Unlimited Custom Layers**

Layers are **fully generic** and string-based. You can create as many layers as you want with any names you want.

**Reserved Layers:**
- `"base"` - The default layer, always active
- `"game_mode"` - Special layer activated during gaming

**Example:**
```ron
layers: {
    "nav": ( remaps: { /* vim navigation */ } ),
    "num": ( remaps: { /* numpad */ } ),
    "symbols": ( remaps: { /* symbols */ } ),
    "coding": ( remaps: { /* IDE shortcuts */ } ),
    "media": ( remaps: { /* media controls */ } ),
    "custom_layer_123": ( remaps: { /* anything */ } ),
}
```

**How it works:**
- Layers stack on top of base layer
- Key lookups check: Game Mode → Current Layer → Base Layer
- Switch layers with `TO("layer_name")`
- Releasing the layer switch key returns to base

**Use cases:**
- Navigation layer (Vim arrows on hjkl)
- Number layer (numpad on home row)
- Symbol layer (programming symbols)
- Media control layer
- IDE/app-specific layers

---

#### 2. **Mod-Tap (MT)**
**formerly "Home Row Mods (HR)"**

**Tap for letter, hold for modifier** - works on **ANY key**, not just home row.

**Syntax:** `MT(tap_key, hold_key)`

**Example:**
```ron
remaps: {
    // Traditional QWERTY home row mods
    KC_A: MT(KC_A, KC_LGUI),   // Tap A, hold for Super
    KC_S: MT(KC_S, KC_LALT),   // Tap S, hold for Alt
    KC_D: MT(KC_D, KC_LCTL),   // Tap D, hold for Ctrl
    KC_F: MT(KC_F, KC_LSFT),   // Tap F, hold for Shift
    
    // Colemak/Dvorak/custom home rows work too!
    KC_T: MT(KC_T, KC_LGUI),
    KC_N: MT(KC_N, KC_LSFT),
    
    // Even non-home-row keys!
    KC_SPC: MT(KC_SPC, KC_LSFT),
    
    // Nested actions - tap for Tab, hold for nav layer
    KC_TAB: MT(KC_TAB, MO("nav")),
}
```

**MT Config (tuning):**
```ron
mt_config: (
    tapping_term_ms: 175,              // How long to wait for hold
    permissive_hold: true,             // If another key pressed, immediately activate modifier
    same_hand_roll_detection: true,    // Rolls on same hand favor tap
    opposite_hand_chord_detection: true, // Chords on opposite hands favor hold
    multi_mod_detection: true,        // Multiple modifiers held simultaneously
    double_tap_then_hold: false,       // Double-tap then hold = repeat tap key
    double_tap_window_ms: 300,         // Window for double-tap detection
    cross_hand_unwrap: true,           // Holding modifier on one hand unwraps MT on other
    adaptive_timing: false,             // Learn user's tap speed and adjust threshold
    roll_detection_window_ms: 150,    // Window for roll detection
    chord_detection_window_ms: 50,     // Window for chord detection
),
```

**Features:**
- **Permissive hold**: If another key is pressed while holding, immediately activates modifier
- **Same-hand roll detection**: Rolls on same hand favor tap (type "as" quickly → both letters)
- **Opposite-hand chord detection**: Chords on opposite hands favor hold
- **Multi-mod detection**: Multiple modifiers held simultaneously all promote to hold
- **Double-tap-and-hold**: Double-tap then hold = repeat the tap key
- **Cross-hand unwrap**: Holding a modifier on one hand unwraps MT keys on the other hand to tap
- **Adaptive timing**: Learns your average tap duration and adjusts threshold automatically
- **Works on ANY key**: Not limited to home row positions
- **Recursive actions**: MT can nest any other actions (even other MT, TO, etc.)

**How it works:**
1. Press MT key → marked as "pending"
2. If another key pressed → resolve based on roll/chord detection
3. If released quickly → emit tap key
4. If held past threshold → emit modifier
5. Double-tap quickly → hold the tap key instead

**Use cases:**
- Reduce finger travel to modifiers
- Custom layouts (Colemak, Dvorak, Workman)
- Ergonomic keyboard layouts
- One-handed typing setups

**Use cases:**
- Reduce finger travel to modifiers
- Custom layouts (Colemak, Dvorak, Workman)
- Ergonomic keyboard layouts
- One-handed typing setups

**Note:** The original OVERLOAD action is now fully covered by MT with configurable settings. Use MT with `permissive_hold: false` for pure timing-based behavior.

---

#### 3. **OneShot Modifier (OSM)**

**Tap once, modifier stays active for exactly one keypress** - perfect for typing capital letters.

**Syntax:** `OSM(modifier_action)`

**Example:**
```ron
remaps: {
    // Tap Shift once, next letter is capitalized
    KC_LSFT: OSM(KC_LSFT),
    KC_LCTL: OSM(KC_LCTL),
    KC_LALT: OSM(KC_LALT),
    KC_LGUI: OSM(KC_LGUI),
}
```

**How it works:**
1. Press OSM key → modifier activates
2. Press any other key → modifier activates with that key, then OSM deactivates
3. If no key pressed within timeout → OSM auto-releases

**Configuration:**
```ron
oneshot_timeout_ms: 5000,  // OSM auto-releases after 5 seconds of idle
```

---

#### 4. **Double-Tap / Tap Dance (DT)**

**Single tap performs one action, double tap performs another** - QMK-style tap dance.

**Syntax:** `DT(single_tap_action, double_tap_action)`

**Example:**
```ron
remaps: {
    // Single tap = Escape, Double tap = Caps Lock (toggle)
    KC_ESC: DT(KC_ESC, TO("game_mode")),
    // Single tap = Alt, Double tap = nav layer
    KC_LALT: DT(KC_LALT, MO("nav")),
}
```

**How it works:**
1. Press key once → starts timer, waits for potential second tap
2. Release before double_tap_window_ms → check for second press
3. Second press detected within window → double tap action triggers
4. No second press → single tap action triggers
5. Timeout expires → single tap action triggers

---

#### 5. **SOCD (Simultaneous Opposite Cardinal Directions)**

**Generic SOCD for gaming** - no longer hardcoded to WASD!

**Syntax:** `SOCD(this_key_action, [opposing_key_actions...])`

**Strategy:** Last Input Priority (LIP) - pressing W then S = S wins, release S = W reactivates

**Example:**
```ron
game_mode: (
    remaps: {
        // WASD SOCD
        KC_W: SOCD(KC_W, [KC_S]),
        KC_S: SOCD(KC_S, [KC_W]),
        KC_A: SOCD(KC_A, [KC_D]),
        KC_D: SOCD(KC_D, [KC_A]),
        
        // Arrow keys SOCD (for racing games!)
        KC_UP: SOCD(KC_UP, [KC_DOWN]),
        KC_DOWN: SOCD(KC_DOWN, [KC_UP]),
        KC_LEFT: SOCD(KC_LEFT, [KC_RGHT]),
        KC_RGHT: SOCD(KC_RGHT, [KC_LEFT]),
    },
)
```

**How it works:**
1. Pressing W → W active
2. While holding W, press S → S takes over (W released, S pressed)
3. Release S → W reactivates automatically
4. Works independently for vertical (W/S) and horizontal (A/D) axes

**Use cases:**
- FPS games (WASD movement)
- Racing games (arrow keys)
- Fighting games (hitbox controllers)
- Any game with opposing direction inputs
- Custom control schemes

---

#### 6. **Command Runner**

**Execute arbitrary shell commands on key press**

**Syntax:** `CMD("command")`

**Example:**
```ron
remaps: {
    KC_F1: CMD("/usr/bin/notify-send 'Hello'"),
    KC_F2: CMD("/usr/bin/playerctl play-pause"),
    KC_F3: CMD("/usr/bin/brightnessctl set 10%+"),
}
```

**Features:**
- **Pure customizability**: Run any shell command
- **Background execution**: Commands run in background thread
- **System integration**: Control media, notifications, brightness, etc.

**Use cases:**
- Media controls (play/pause, volume, skip)
- System commands (screenshot, lock, sleep)
- Custom scripts and automation
- Notification triggers

---

#### 7. **Layer Switching**

**Layer keys** (each takes a layer name in quotes):

| Action | Behavior |
|---|---|
| `MO("layer")` | Momentary, like QMK's `MO`: the layer is on while the key is held, then turns off |
| `TG("layer")` | Toggle: press to turn on, press again to turn off. Nothing is held |
| `LATCH("layer")` | Turn on and keep on. Pressing it again does nothing. Only `TO("base")` or `TG` turns it off |
| `TO("layer")` | Exclusive, like QMK's `TO`: turns every other layer off, then turns this one on and keeps it on. `TO("base")` leaves every layer and returns to the base layer |

> **Breaking change:** `TO` used to be a hold action. Hold layers now need `MO`. `SWITCH` was removed; `TO` does its job.
| `Transparent` | Fall through: look this key up on the layers below |

**Example:**
```ron
remaps: {
    KC_LALT: MO("nav"),              // Hold Left Alt → nav layer
    KC_F9: TG("nohrm"),              // Toggle nohrm on and off
    KC_F1: LATCH("steno"),           // Steno on, and stays on
    KC_F10: TO("base"),              // Leave steno
    KC_F8: TO("stenopractice"),  // Only stenopractice is on afterwards
}
```

**How the layer stack works:**
1. Active layers form a stack. The top one is the current layer. Only the current layer's steno engine captures strokes.
2. A key lookup walks the stack from the top down and uses the first remap it finds. A layer that doesn't remap a key lets it fall through to the layers below.
3. When nothing on the stack remaps the key, game-mode remaps apply (if game mode is on), then the base `remaps`.
4. Turning on a layer that is already on does nothing. It is not added to the stack twice.
5. Layer keys don't turn each other off. Stacking `TG("nohrm")` on top of steno leaves steno underneath, but steno is no longer the current layer, so its strokes stop being captured. Use `TO` to reset the stack: `TO("base")` clears it.
6. A held `TO`/`MO` key only removes its own layer when released. Anything switched on underneath it stays on.
7. `default_layer` (global, or per keyboard) puts one layer on the stack at startup, above base.

Layer actions also work inside `MULTI`, `DT`, `MT` and `SOCD`, so a layer can be held as part of a combined action.

---

#### 8. **Simple Key Remapping**

**Remap any key to any other key**

**Syntax:** `KC_OUTPUT` (the Key() wrapper is optional via preprocessor)

**Example:**
```ron
remaps: {
    KC_CAPS: KC_ESC,        // Caps Lock → Escape
    KC_ESC: KC_GRV,         // Escape → Backtick
    KC_RGHT: KC_BSPC,       // Right Arrow → Backspace
}
```

---

#### 9. **Game Mode (Automatic Detection)**

**Special layer activated during gaming** - auto-detected or manual toggle

**Auto-detection methods** (`src/niri/gamemode_detection.rs::detect_game_mode`,
checked in order, first match wins):
1. **Gamescope App ID**: app_id is exactly `gamescope`
2. **Steam App Prefix**: app_id starts with `steam_app_`
3. **Wine games**: app_id is `wine`/`wine-*`/ends in `.exe`, **plus** a
   secondary confirmation (`looks_like_game_install_path`) that the
   process's cwd or cmdline contains a known game-library fragment
   (`steamapps/common`, GOG, Epic, itch.io, Lutris/Heroic, `Games/`).
   `/proc/{pid}/exe` itself is useless here - under Wine/Proton it just
   resolves to the wine loader binary, not the guest .exe, which is why
   cwd/cmdline are checked instead. Without this second signal, a bare
   `.exe` app_id only proves "Windows binary," not "game" (a Wine-run
   installer or utility would match too) - deliberately biased toward a
   false negative (leave it alone) over a false positive.
4. **Roblox / Epic Games / Lutris / Heroic / Sober**: known app_id patterns
5. **.NET games**: app_id is `dotnet`, **and** matched against known game
   titles - an unrecognized title is left as Normal, not assumed (same
   false-negative bias as #3).
6. **IS_GAME env var**: process has `IS_GAME=1` in its environment
7. **Process tree walk**: walks 10 levels up to find `gamescope`/`gamemode`(d)

Two-tier manual override system (`src/gamemode_state.rs`,
`AsyncDaemon::resolve_effective_game_mode` in `src/daemon/daemon.rs`), both
session-only (reset on daemon restart):
- **Window override** (`keymux gamemode window on|off|toggle|auto`): forces
  game mode for one specific app_id, on top of the heuristics above.
- **Global override** (`keymux gamemode global always-on|always-off|auto`):
  ignores every per-window rule/override entirely.

Precedence, highest first: global override → window override → heuristics.

**Configuration:**
```ron
game_mode: (
    remaps: {
        // Disable MT mods in games (use plain Key)
        KC_A: KC_A,
        KC_S: KC_S,
        KC_D: KC_D,
        KC_F: KC_F,
        
        // Add SOCD
        KC_W: SOCD(KC_W, [KC_S]),
        KC_S: SOCD(KC_S, [KC_W]),
        KC_A: SOCD(KC_A, [KC_D]),
        KC_D: SOCD(KC_D, [KC_A]),
        
        // Keep essential remaps
        KC_CAPS: KC_ESC,
    },
)
```

**How it works:**
1. A per-compositor watcher process (Niri/Hyprland/Sway/i3/bspwm) reports
   every window focus change to the root daemon over IPC
2. The root daemon resolves the effective state (overrides, then heuristics)
3. Automatically enables/disables the game_mode layer
4. Game mode has highest priority in lookup

**Use cases:**
- Disable home row mods (prevent accidental modifier activation)
- Enable SOCD for movement keys
- Gaming-specific bindings
- Different sensitivity for gaming vs typing

---

### Advanced Features

#### 9. **Per-Keyboard Overrides**

**Different configs for different keyboards**

**Example:**
```ron
keyboard_overrides: {
    "1234:5678:0100:0003:usb-...": (
        settings: (
            tapping_term_ms: 150,  // Faster for this keyboard
        ),
        keymap: (
            base_remaps: {
                KC_CAPS: KC_LCTL,  // Different for this one
            },
            layers: {
                "nav": ( remaps: { /* custom nav */ } ),
            },
            game_mode_remaps: {
                KC_W: KC_UP,  // Different game binds
            },
        ),
    ),
}
```

**Use cases:**
- Laptop keyboard vs external keyboard
- Gaming keyboard vs typing keyboard
- Ergonomic split keyboard with different layout
- Testing new configs without affecting main keyboard

---

#### 10. **Timing Customization**

**Fine-tune tap/hold behavior per keyboard**

**Settings:**
```ron
// Global settings
tapping_term_ms: 175,              // How long to wait for hold
double_tap_window_ms: Some(300),   // Window for double-tap detection

// Per-keyboard override
keyboard_overrides: {
    "keyboard-id": (
        settings: (
            tapping_term_ms: 150,            // Faster
            double_tap_window_ms: Some(250), // Quicker
        ),
    ),
}
```

**Recommendations:**
- **130-150ms**: Aggressive (fast typers, gaming keyboards)
- **175-200ms**: Balanced (most users)
- **200-250ms**: Conservative (slow/deliberate typers)

---

### Steno (Plover-compatible)

Steno is a layer kind: `kind: Steno((...))`. A steno layer captures its stroke keys instead of remapping them, groups them into strokes, and types the Plover dictionary text for each stroke.

**Mechanics:**
- **Strokes**: a stroke is the set of stroke keys pressed together. It completes when the keys come up. `stroke_timeout_ms` (default 5000) completes a stroke whose key never comes up.
- **Translation**: each completed stroke is translated immediately. A multi-stroke entry such as `KPA/TKAOEU` replaces the text of the earlier strokes when it matches, so text already typed can be rewritten (retro-correction).
- **Undo**: the stroke `*` reverses the last translation, including any retro-correction. Space and Backspace are swallowed while there is text to undo, and act normally once nothing is left.
- **Placeholder**: a stroke that matches nothing on its own but starts a longer entry types `_`. The next stroke either completes the entry and replaces the `_`, or doesn't and the `_` is removed. Space and undo also remove it.
- **Unmatched strokes**: type nothing, and appear as `?` on the tape.
- **Delete** always reaches the screen, even on a steno layer.
- **Dictionaries**: Plover JSON files, highest priority first. They are parsed once and cached as a binary file in `steno/cache`, keyed by the sha256 of each source file, so a changed dictionary is re-parsed automatically.
- **Tape**: every stroke is logged to `~/.local/state/keymux/steno-tape.txt`. `keymux steno tape -f` follows it live.

---

### Supported Keys

**All standard keyboard keys supported:**

- **Letters**: A-Z
- **Numbers**: 0-9, Numpad 0-9
- **Modifiers**: Ctrl, Shift, Alt, Super/Win/Cmd (left & right)
- **Function**: F1-F24
- **Navigation**: Arrows, Home, End, Page Up/Down, Insert, Delete
- **Editing**: Backspace, Enter, Tab, Space, Escape
- **Punctuation**: All standard symbols
- **Numpad**: All numpad keys + Num Lock
- **Locking**: Caps Lock, Scroll Lock
- **Media**: Play/Pause, Stop, Next, Previous, Volume, Mute
- **System**: Power, Sleep, Wake, Calculator, My Computer
- **Web**: Browser controls, Search, Home, Back, Forward, Refresh
- **Application**: Menu, App key

**Total: 150+ keys supported**

---

## Design Goals

---

## Technical Architecture

### Architecture Components

```
┌─────────────────┐    ┌──────────────────┐    ┌─────────────────┐
│   Root Daemon   │◄──►│  Session Manager │◄──►│  User Sessions  │
│  (Async Event)  │    │   (Ownership)    │    │   (Per User)    │
└─────────────────┘    └──────────────────┘    └─────────────────┘
         │                       │                       │
         ▼                       ▼                       ▼
┌─────────────────┐    ┌──────────────────┐    ┌─────────────────┐
│ Device Pool     │    │  Config Manager  │    │   IPC Server   │
│ (Hotplug Mgr)   │    │  (Hot Reload)    │    │  (Multi-user)  │
└─────────────────┘    └──────────────────┘    └─────────────────┘
         │                       │                       │
         ▼                       ▼                       ▼
┌─────────────────┐    ┌──────────────────┐    ┌─────────────────┐
│ Thread Pool     │    │  Game Mode Mgr   │    │  Niri Monitor   │
│ (Per Device)    │    │ (Smart Detection)│    │ (Auto-restart) │
└─────────────────┘    └──────────────────┘    └─────────────────┘
```

### Data Flow: Key Press Journey

```
1. Physical Keyboard
         ↓
2. evdev Device (/dev/input/eventX)
         ↓  
3. Event Processor Thread (GRABBED - exclusive access)
         ↓
4. Keymap Processor (zero-latency processing)
         ↓
   ┌─────────────────────────────┐
   │  Lookup Priority:           │
   │  1. Game Mode (if active)   │
   │  2. Current Layer           │
   │  3. Base Layer              │
   └─────────────────────────────┘
         ↓
5. Action Processing
    ├─ Key(x) → Emit key x
    ├─ MT(tap, hold) → Mod-Tap logic with roll/chord detection
    ├─ OSM(modifier) → OneShot Modifier
    ├─ DT(tap, dtap) → Double-Tap / Tap Dance
    ├─ MO/TG/LATCH/TO(layer) → Layer momentary/toggle/latch/exclusive switch
    ├─ Transparent → Fall through to lower layer
    ├─ SOCD() → SOCD resolution
    └─ CMD(command) → Execute shell command
         ↓
6. Virtual uinput Device
         ↓
7. Desktop Environment / Applications
```

### Thread Model

#### One Thread Per Event File

Each keyboard can expose multiple event files (e.g., `/dev/input/event3` for keys, `/dev/input/event4` for media keys). We spawn **one dedicated thread per event file** for maximum parallelism.

**Example:**
```
Keyboard "Keychron K2"
  ├─ Thread 1: /dev/input/event3 (main keys)
  └─ Thread 2: /dev/input/event4 (media keys)

Keyboard "Built-in Laptop"
  └─ Thread 3: /dev/input/event5 (all keys)
```

**Why:**
- Some keyboards split functionality across multiple event files
- Parallel processing for complex keyboards
- Prevents one device from blocking another

#### Thread Lifecycle

1. **Spawn**: Daemon creates thread when keyboard assigned to user
2. **Run**: Thread processes keys in tight loop with `yield_now()`
3. **Commands**: Non-blocking checks for game mode / shutdown via channels
4. **Shutdown**: Graceful cleanup on user logout or device disconnect
5. **No restart on periodic checks**: Threads only restart on actual hardware changes

#### State Preservation

Game mode state is preserved across thread restarts:
1. Stored in AsyncDaemon struct
2. Sent to new threads on creation
3. Prevents game mode resets during session changes

---

### Keymap Processing Logic

#### Mod-Tap (MT) Processing (Detailed)

**State Machine:**
```
┌──────────────┐
│   Key Press  │
└──────┬───────┘
       │
       ▼
┌──────────────────────┐
│  Is Double-Tap?      │ ◄── Check last tap time
├──────┬───────────────┤
│ YES  │ NO            │
▼      ▼               │
Hold   Pending         │
Base   State   ◄───────┘
Key                    
       │               
       ├─ Other Key Pressed? → Resolve to HOLD (permissive)
       │
       ├─ Timeout? → Resolve to HOLD
       │  
       └─ Released? → TAP
```

**Data Structures:**
```rust
struct KeymapProcessor {
    held_keys: HashMap<KeyCode, Vec<KeyAction>>,
    hrm_last_tap: HashMap<KeyCode, Instant>,  // Generic, not fixed array
    pending_hrm: HashSet<KeyCode>,             // Generic, any key can be HR mod
    double_tap_window_ms: u32,
    // ...
}
```

**No hardcoded key restrictions** - works on any KeyCode!

---

#### SOCD Resolution (Detailed)

**Generic State Machine:**
```rust
struct SocdState {
    // Track all SOCD pairs dynamically
    held_keys: HashMap<KeyCode, bool>,           // Which keys held
    opposing_pairs: HashMap<KeyCode, KeyCode>,   // Key → its opposite
    last_input: HashMap<KeyCode, KeyCode>,       // For each pair, which was last
    active_keys: HashMap<KeyCode, Option<KeyCode>>, // Currently emitted key per pair
}
```

**Example for W/S pair:**
```
State: W=false, S=false, Active=None

1. Press W
   → W=true, Active=W, Emit: W press

2. Press S (while holding W)  
   → S=true, Active=S, last=S
   → Emit: W release, S press

3. Release S (still holding W)
   → S=false, Active=W, last=W  
   → Emit: S release, W press

4. Release W
   → W=false, Active=None
   → Emit: W release
```

**Supports:**
- Any opposing key pairs (not just WASD)
- Multiple independent pairs (W/S, A/D, Up/Down, Left/Right, custom)
- Last Input Priority (LIP) strategy
- Clean state management per pair

---

### Configuration System

#### RON Format

We use **RON (Rusty Object Notation)** - Rust's equivalent to JSON but more ergonomic:

**Why RON:**
- Native Rust data structure syntax
- Comments supported (`//`)
- Trailing commas allowed
- More readable than JSON for configs
- Strong typing with serde

**Example:**
```ron
(
    tapping_term_ms: 175,
    remaps: {
        KC_A: MT(KC_A, KC_LGUI),
        KC_CAPS: MO("nav"),
    },
    layers: {
        "nav": (
            remaps: {
                KC_H: KC_LEFT,
                KC_J: KC_DOWN,
            },
        ),
    },
)
```

#### Per-Keyboard Overrides

**Keyboard ID format:**
```
{vendor}:{product}:{version}:{interface}:usb-{bus}-{port}/input{N}
Example: 1234:5678:0100:0003:usb-0000:00:14.0-1/input0
```

**Override resolution:**
1. Load base config
2. If keyboard ID matches override, apply overrides:
   - Settings override global settings
   - Keymap overrides replace (not merge) base keymap
3. Result is keyboard-specific config

#### Config Validation

**Validation checks:**
- All referenced layers exist
- Key codes are valid
- SOCD pairs are symmetric (if W→S, then S→W)
- No circular layer references
- Timing values are reasonable (> 0ms, < 1000ms)

---

---

## Implementation Roadmap

All phases (1-7) have been fully implemented. The codebase includes:

- ✅ Generic string-based layers
- ✅ Generic SOCD with any opposing key pairs
- ✅ MT (Mod-Tap) with any key support
- ✅ 150+ KeyCodes (letters, numbers, modifiers, F-keys, navigation, numpad, media, international)
- ✅ Full SOCD state management with HashMap
- ✅ Adaptive timing with predictive scoring
- ✅ Directory restructure in `event_processor/`

---

## Implementation Details

## Key Design Decisions

### 1. Hybrid Sync/Async Architecture

**Hot Path (Synchronous)** - MAXIMUM SPEED
```
Physical Device → Event Processor (sync) → Virtual Device
         ↓                ↓                    ↓
   evdev::fetch()  Keymap Processing    evdev::emit()
   (non-blocking)   (zero alloc)       (instant)
```

**Cold Path (Asynchronous)** - ROBUST MANAGEMENT
```
Root Daemon (async) ←→ IPC Server ←→ Config Manager
      ↓                    ↓              ↓
Hotplug Events      User Commands    File Watcher
Session Changes     Status Queries   Hot Reload
```

### 2. Session-Based Keyboard Ownership

- **First-come-first-serve**: Keyboards assigned to first requesting active user
- **Automatic release**: Keyboards freed when user session ends
- **Priority system**: Admin/system users can override regular users
- **Multi-session support**: Multiple users can have different keyboards

### 3. Smart Hot-Reload

**Before (Destructive)**:
```
Config Change → Kill ALL Threads → Reload Config → Restart ALL Threads
```

**After (Smart)**:
```
Config Change → Analyze Changes → Restart ONLY Affected Threads
```

### 4. Thread Lifecycle Management

- **Graceful shutdown**: Threads receive signals and clean up properly
- **Device release**: Proper ungrab before thread exit
- **Zombie cleanup**: Automatic detection and cleanup of dead threads
- **Resource tracking**: Monitor thread health and device state

## Component Details

### Session Manager (`src/session_manager.rs`)

**Purpose**: Track which users are logged in, so each keyboard belongs to an active session.

- Reads `loginctl` every 5 seconds (`refresh_sessions`).
- `is_user_active(uid)` and `get_active_uids()` answer ownership questions.

### Daemon (`src/daemon/daemon.rs`, `AsyncDaemon`)

**Purpose**: Orchestrate devices, configuration and processors.

- `run()` loops with `tokio::select!` over hotplug events (debounced), IPC commands, config changes, the 5-second session check, and reports from processor threads that have exited.
- Each enabled keyboard gets its own event-processor thread. If one exits, the daemon cleans up that keyboard so it can be attached again.

### Event Processor (`src/event_processor/`)

**Purpose**: Turn physical key events into output keys, synchronously.

- `run_processor` starts one thread per keyboard, grabs its device, and writes the results to a virtual keyboard created with uinput.
- `KeymapProcessor` (`keymap.rs`) holds the layer stack, the state of every held key, the MT/DT/OSM/SOCD processors and the steno engines. Each key press is resolved when it goes down and remembered until it comes up.
- `LayerStack` (`layer_stack.rs`) is the ordered list of active layers described under Layer Switching.
- `AltF12` (`mod.rs`) watches the first key press after a keyboard attaches for the Alt+F12 release chord.
- Key handling is synchronous: no async work happens per key press.

### Config (`src/config/`)

- `config.rs`: the RON schema, its defaults, and `for_keyboard`, which merges per-keyboard overrides into the global config.
- `validator.rs`: what `keymux validate` checks: SOCD pairs, timing ranges, layer references (including nested ones), steno layouts and dictionary paths.
- `config_manager.rs`: loads the config file for the daemon.

### Steno (`src/steno/`)

See the Steno section above.

---

## Performance Characteristics

### Target Metrics

- **Key latency**: < 1ms (hot path priority)
- **Hot-reload**: < 100ms without interruption
- **Device discovery**: < 50ms on hotplug
- **Session tracking**: < 10ms for ownership changes

### Memory Usage

- **Per thread**: Minimal stack + keymap state
- **Shared structures**: Arc<RwLock> for thread-safe access
- **Zero-copy**: Event passing without allocations where possible

## Error Handling Strategy

### Self-Healing Components

1. **Device hotplug**: Automatic rediscovery on connection loss
2. **Thread restart**: Dead threads automatically respawned
3. **Config recovery**: Invalid configs rejected with rollback
4. **Session recovery**: Reclaim keyboards on session restoration

### Graceful Degradation

- **Partial failures**: Single keyboard issues don't affect others
- **Fallback modes**: Continue with reduced functionality
- **User notification**: Clear error reporting and recovery steps

## Security Considerations

### User Isolation

- **Device permissions**: Only owner can control keyboard
- **IPC validation**: Verify user permissions for commands
- **Session boundaries**: Respect user session limits

### Input Validation

- **Config sanitization**: Prevent malicious configurations
- **Event filtering**: Drop malformed input events
- **Resource limits**: Prevent resource exhaustion

## Deployment

### systemd Integration

There is exactly **one** root daemon process, ever - `keymux daemon` always
runs as `User=root` (it hard-refuses to start otherwise, see
`AsyncDaemon::new`). It is not per-user: internally it multiplexes every
currently logged-in user's own `~/.config/keymux/config.ron` via
`SessionManager` + a `HashMap<uid, ConfigManager>`, attributing each keyboard
to whichever session currently owns it. There is no separate "user daemon"
binary or mode - `keymux daemon --user <name>` does not exist.

**Root daemon service** (system-wide, one instance):
```ini
[Unit]
Description=Keyboard Middleware Root Daemon
After=graphical-session.target

[Service]
Type=simple
ExecStart=/usr/bin/keymux daemon
Restart=always
RestartSec=5
User=root

[Install]
WantedBy=multi-user.target
```
```bash
sudo systemctl daemon-reload
sudo systemctl enable --now keymux.service
```

**Compositor watcher service** (per-user, one per compositor - this is what
was previously mislabeled a "user daemon"): a lightweight process
(`keymux niri-daemon`, `-hyprland-daemon`, `-sway-daemon`, `-i3-daemon`,
`-bspwm-daemon`) that watches window focus changes in *your* session and
reports them to the root daemon over IPC for game mode detection. Install
only the one matching your compositor.
```ini
[Unit]
Description=Keyboard Middleware Niri Watcher
PartOf=graphical-session.target

[Service]
Type=simple
ExecStart=/usr/bin/keymux niri-daemon
Restart=on-failure

[Install]
WantedBy=default.target
```
```bash
systemctl --user daemon-reload
systemctl --user enable --now keymux-niri.service
```

## Migration Path

### Phase 1: Core Architecture
- Implement async daemon skeleton
- Add session manager
- Migrate existing event processor

### Phase 2: Smart Features  
- Smart hot-reload implementation
- Enhanced error recovery
- Performance optimizations

### Phase 3: Advanced Features
- Multi-user session support
- Advanced game mode detection
- Configuration validation

## Future Enhancements

### Performance
- **NUMA awareness**: Bind threads to CPU cores
- **Real-time scheduling**: High priority for key processing
- **Memory mapping**: Zero-copy event handling

### Features
- **Device groups**: Logical keyboard groupings
- **Dynamic layers**: Runtime layer creation
- **Macro recording**: User-defined key sequences

### Integration
- **Wayland native**: Direct compositor integration
- **PulseAudio sync**: Audio-visual feedback coordination
- **Network support**: Remote keyboard management

---

This architecture provides a solid foundation for high-performance, multi-user keyboard middleware with robust error handling and future extensibility.
