//! SOCD (Simultaneous Opposite Cardinal Direction).
//!
//! Of a set of opposing keys, the one pressed most recently is the only one the application
//! sees down. Letting it go hands the press back to the one still held.

use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{EmitResult, HandleContext, HeldAction};
use crate::keycode::KeyCode;
use std::collections::HashMap;

/// One set of opposing keys: the ones physically held, oldest first. The last is the one
/// the application sees down.
#[derive(Debug, Default)]
struct Group {
    held: Vec<KeyCode>,
}

impl Group {
    fn active(&self) -> Option<KeyCode> {
        self.held.last().copied()
    }

    fn press(&mut self, key: KeyCode) -> Vec<(KeyCode, bool)> {
        let before = self.active();
        if !self.held.contains(&key) {
            self.held.push(key);
        }
        transition(before, self.active())
    }

    fn release(&mut self, key: KeyCode) -> Vec<(KeyCode, bool)> {
        let before = self.active();
        self.held.retain(|&held| held != key);
        transition(before, self.active())
    }
}

/// The key events that take a group from one active key to another: the old one up, then the
/// new one down. Nothing when the active key stays.
fn transition(before: Option<KeyCode>, after: Option<KeyCode>) -> Vec<(KeyCode, bool)> {
    if before == after {
        return Vec::new();
    }
    before
        .map(|key| (key, false))
        .into_iter()
        .chain(after.map(|key| (key, true)))
        .collect()
}

pub struct SocdProcessor {
    key_to_group: HashMap<KeyCode, usize>,
    groups: Vec<Group>,
}

impl SocdProcessor {
    /// One group per SOCD action in any remap table, keyed by the keys it names. An action
    /// naming a key that is already in a group joins that group, so the two halves of a pair,
    /// and the same pair repeated on several layers, share one.
    pub fn from_config(config: &Config) -> Self {
        let mut groups: Vec<Group> = Vec::new();
        let mut key_to_group = HashMap::new();
        let tables = std::iter::once(&config.remaps)
            .chain(config.layers.values().map(|layer| &layer.remaps))
            .chain(std::iter::once(&config.game_mode.remaps));
        for action in tables.flat_map(HashMap::values) {
            let KeyAction::SOCD(this, opposing) = action else {
                continue;
            };
            let Some(this_key) = this.as_keycode() else {
                continue;
            };
            let keys: Vec<KeyCode> = std::iter::once(this_key)
                .chain(opposing.iter().filter_map(|other| other.as_keycode()))
                .collect();
            if keys.len() < 2 {
                continue;
            }
            let id = keys
                .iter()
                .find_map(|key| key_to_group.get(key).copied())
                .unwrap_or_else(|| {
                    groups.push(Group::default());
                    groups.len() - 1
                });
            for key in keys {
                key_to_group.insert(key, id);
            }
        }
        Self {
            key_to_group,
            groups,
        }
    }

    fn group(&mut self, key: KeyCode) -> Option<&mut Group> {
        let id = *self.key_to_group.get(&key)?;
        self.groups.get_mut(id)
    }

    /// `key` goes down: the events that makes the application see.
    pub fn press(&mut self, key: KeyCode) -> Vec<(KeyCode, bool)> {
        self.group(key)
            .map_or_else(Vec::new, |group| group.press(key))
    }

    /// `key` comes up: the events that makes the application see.
    pub fn release(&mut self, key: KeyCode) -> Vec<(KeyCode, bool)> {
        self.group(key)
            .map_or_else(Vec::new, |group| group.release(key))
    }
}

/// Press the key a SOCD action names. What is held is that key, not the physical one, since
/// that is what the release must take out of the group.
pub fn emit_socd(
    action: &KeyAction,
    ctx: &mut HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    let KeyAction::SOCD(this, _) = action else {
        return (EmitResult::None, None);
    };
    let Some(key) = this.as_keycode() else {
        return (EmitResult::None, None);
    };
    (
        EmitResult::EmitKeys(ctx.socd_processor.press(key)),
        Some(HeldAction::SocdManaged(key)),
    )
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::event_processor::{KeymapProcessor, ProcessResult};
    use crate::keycode::KeyCode;
    use std::path::PathBuf;

    fn processor(remaps: &str) -> KeymapProcessor {
        let config: Config = ron::from_str(&format!("(remaps: {{ {remaps} }})")).unwrap();
        KeymapProcessor::new(&config, PathBuf::from("config.ron"), 0)
    }

    #[test]
    fn the_key_pressed_last_wins_and_hands_back_on_release() {
        let mut p =
            processor("KC_A: SOCD(Key(KC_A), [Key(KC_D)]), KC_D: SOCD(Key(KC_D), [Key(KC_A)]),");
        assert_eq!(
            p.process_key(KeyCode::KC_A, true),
            ProcessResult::EmitKey(KeyCode::KC_A, true)
        );
        assert_eq!(
            p.process_key(KeyCode::KC_D, true),
            ProcessResult::MultipleEvents(vec![(KeyCode::KC_A, false), (KeyCode::KC_D, true)])
        );
        assert_eq!(
            p.process_key(KeyCode::KC_D, false),
            ProcessResult::MultipleEvents(vec![(KeyCode::KC_D, false), (KeyCode::KC_A, true)])
        );
        assert_eq!(
            p.process_key(KeyCode::KC_A, false),
            ProcessResult::EmitKey(KeyCode::KC_A, false)
        );
    }

    /// The physical key and the key it sends differ: the release must let go of the one that
    /// was sent, or it stays down for good.
    #[test]
    fn a_socd_key_that_sends_another_key_lets_it_go_on_release() {
        let mut p = processor(
            "KC_A: SOCD(Key(KC_LEFT), [Key(KC_RGHT)]), \
             KC_D: SOCD(Key(KC_RGHT), [Key(KC_LEFT)]),",
        );
        assert_eq!(
            p.process_key(KeyCode::KC_A, true),
            ProcessResult::EmitKey(KeyCode::KC_LEFT, true)
        );
        assert_eq!(
            p.process_key(KeyCode::KC_A, false),
            ProcessResult::EmitKey(KeyCode::KC_LEFT, false)
        );
    }

    #[test]
    fn a_key_in_no_group_is_not_touched() {
        let mut p =
            processor("KC_A: SOCD(Key(KC_A), [Key(KC_D)]), KC_D: SOCD(Key(KC_D), [Key(KC_A)]),");
        assert_eq!(
            p.process_key(KeyCode::KC_W, true),
            ProcessResult::EmitKey(KeyCode::KC_W, true)
        );
    }
}
