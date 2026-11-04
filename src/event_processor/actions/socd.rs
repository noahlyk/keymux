use crate::config::{Config, KeyAction};
use crate::event_processor::actions::{EmitResult, HeldAction, ProcessResult};
use crate::keycode::KeyCode;
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocdResolution {
    EmitKey(KeyCode, bool),
    MultipleEvents(Vec<(KeyCode, bool)>),
    None,
}

#[derive(Debug, Clone)]
pub struct SocdGroup {
    held_stack: Vec<KeyCode>,
    active_key: Option<KeyCode>,
}

impl SocdGroup {
    pub fn new(_all_keys: Vec<KeyCode>) -> Self {
        Self {
            held_stack: Vec::new(),
            active_key: None,
        }
    }

    pub fn on_press(&mut self, keycode: KeyCode) -> Option<(Option<KeyCode>, Option<KeyCode>)> {
        let old_active = self.active_key;

        if !self.held_stack.contains(&keycode) {
            self.held_stack.push(keycode);
        }

        let new_active = self.held_stack.last().copied();
        self.active_key = new_active;

        (old_active != new_active).then_some((old_active, new_active))
    }

    pub fn on_release(&mut self, keycode: KeyCode) -> Option<(Option<KeyCode>, Option<KeyCode>)> {
        let old_active = self.active_key;

        self.held_stack.retain(|&k| k != keycode);

        let new_active = self.held_stack.last().copied();
        self.active_key = new_active;

        (old_active != new_active).then_some((old_active, new_active))
    }
}

pub struct SocdProcessor {
    key_to_group: HashMap<KeyCode, usize>,
    groups: Vec<SocdGroup>,
}

impl SocdProcessor {
    pub fn new(
        socd_definitions: HashMap<KeyCode, Vec<KeyCode>>,
    ) -> (Self, HashMap<KeyCode, usize>, Vec<SocdGroup>) {
        let mut groups = Vec::new();
        let mut key_to_group = HashMap::new();

        for (this_key, opposing_keys) in socd_definitions {
            let mut all_keys = vec![this_key];
            all_keys.extend(opposing_keys);

            let group_id = groups.len();
            groups.push(SocdGroup::new(all_keys.clone()));

            for key in all_keys {
                key_to_group.insert(key, group_id);
            }
        }

        let processor = Self {
            key_to_group: key_to_group.clone(),
            groups: groups.clone(),
        };

        (processor, key_to_group, groups)
    }

    pub fn on_press(&mut self, keycode: KeyCode) -> Option<(Option<KeyCode>, Option<KeyCode>)> {
        if let Some(&group_id) = self.key_to_group.get(&keycode) {
            if let Some(group) = self.groups.get_mut(group_id) {
                return group.on_press(keycode);
            }
        }
        None
    }

    pub fn on_release(&mut self, keycode: KeyCode) -> Option<(Option<KeyCode>, Option<KeyCode>)> {
        if let Some(&group_id) = self.key_to_group.get(&keycode) {
            if let Some(group) = self.groups.get_mut(group_id) {
                return group.on_release(keycode);
            }
        }
        None
    }
}

impl SocdProcessor {
    pub fn from_config(config: &Config) -> Self {
        let socd_definitions = build_socd_definitions(config);
        let (processor, _, _) = Self::new(socd_definitions);
        processor
    }

    pub fn handle_press(&mut self, keycode: KeyCode) -> SocdResolution {
        if let Some((old_active, new_active)) = self.on_press(keycode) {
            generate_socd_transition(old_active, new_active)
        } else {
            SocdResolution::None
        }
    }

    pub fn handle_release(&mut self, keycode: KeyCode) -> SocdResolution {
        if let Some((old_active, new_active)) = self.on_release(keycode) {
            generate_socd_transition(old_active, new_active)
        } else {
            SocdResolution::None
        }
    }
}

fn build_socd_definitions(config: &Config) -> HashMap<KeyCode, Vec<KeyCode>> {
    let mut socd_definitions: HashMap<KeyCode, Vec<KeyCode>> = HashMap::new();
    let extract_socd = |remaps: &HashMap<KeyCode, KeyAction>,
                        defs: &mut HashMap<KeyCode, Vec<KeyCode>>| {
        for action in remaps.values() {
            if let KeyAction::SOCD(this_action, opposing_actions) = action {
                if let KeyAction::Key(this_key) = this_action.as_ref() {
                    let mut opposing_keys = Vec::new();
                    for opp_action in opposing_actions {
                        if let KeyAction::Key(opp_key) = opp_action.as_ref() {
                            opposing_keys.push(*opp_key);
                        }
                    }
                    if !opposing_keys.is_empty() {
                        defs.insert(*this_key, opposing_keys);
                    }
                }
            }
        }
    };
    extract_socd(&config.remaps, &mut socd_definitions);
    for layer_config in config.layers.values() {
        extract_socd(&layer_config.remaps, &mut socd_definitions);
    }
    extract_socd(&config.game_mode.remaps, &mut socd_definitions);
    socd_definitions
}

fn generate_socd_transition(
    old_active: Option<KeyCode>,
    new_active: Option<KeyCode>,
) -> SocdResolution {
    match (old_active, new_active) {
        (None, None) => SocdResolution::None,
        (None, Some(new_key)) => SocdResolution::EmitKey(new_key, true),
        (Some(old_key), None) => SocdResolution::EmitKey(old_key, false),
        (Some(old_key), Some(new_key)) if old_key == new_key => SocdResolution::None,
        (Some(old_key), Some(new_key)) => {
            SocdResolution::MultipleEvents(vec![(old_key, false), (new_key, true)])
        }
    }
}

const fn extract_keycode(action: &KeyAction) -> Option<KeyCode> {
    match action {
        KeyAction::Key(kc) => Some(*kc),
        _ => None,
    }
}

pub fn handle_socd_action(
    socd_processor: &mut SocdProcessor,
    _keycode: KeyCode,
    this_action: &KeyAction,
) -> SocdResolution {
    extract_keycode(this_action).map_or(SocdResolution::None, |this_key| {
        socd_processor.handle_press(this_key)
    })
}

pub fn emit_socd(
    action: &KeyAction,
    _keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> (EmitResult, Option<HeldAction>) {
    match action {
        KeyAction::SOCD(this_action, _) => {
            let this_key = this_action.as_keycode();
            if let Some(key) = this_key {
                let result = handle_socd_action(ctx.socd_processor, key, this_action);
                (result.into(), Some(HeldAction::SocdManaged))
            } else {
                (EmitResult::None, Some(HeldAction::SocdManaged))
            }
        }
        _ => (EmitResult::None, None),
    }
}

pub fn unemit_socd(
    action: &KeyAction,
    held_action: HeldAction,
    keycode: KeyCode,
    ctx: &mut super::HandleContext<'_>,
) -> EmitResult {
    match (action, held_action) {
        (KeyAction::SOCD(_, _), HeldAction::SocdManaged) => {
            let result: ProcessResult = ctx.socd_processor.handle_release(keycode).into();
            match result {
                ProcessResult::EmitKey(kc, pressed) => EmitResult::EmitKey(kc, pressed),
                ProcessResult::MultipleEvents(events) => EmitResult::EmitKeys(events),
                _ => EmitResult::None,
            }
        }
        _ => EmitResult::None,
    }
}
