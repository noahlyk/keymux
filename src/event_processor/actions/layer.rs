use crate::config::KeyAction;
use crate::event_processor::actions::{EmitResult, HeldAction};
use crate::event_processor::layer_stack::LayerStack;
use crate::keycode::KeyCode;

pub fn emit_layer(
    action: &KeyAction,
    _keycode: KeyCode,
    layer_stack: &mut LayerStack,
) -> (EmitResult, Option<HeldAction>) {
    match action {
        // Leaving the layers that were switched into, e.g. steno, back to the base layer
        KeyAction::TO(layer) if layer.is_base() => {
            layer_stack.leave_to_base();
            (EmitResult::LayerAction(layer.clone()), None)
        }
        // Exclusive, like QMK's TO: every other layer goes, and this one stays on
        KeyAction::SWITCH(layer) => {
            layer_stack.leave_to_base();
            layer_stack.activate_layer(layer.clone());
            (EmitResult::LayerAction(layer.clone()), None)
        }
        // Stays on after the key is released; only TO("base") or another TG turns it off
        KeyAction::LATCH(layer) => {
            layer_stack.activate_layer(layer.clone());
            (EmitResult::LayerAction(layer.clone()), None)
        }
        KeyAction::TO(layer) => {
            layer_stack.activate_layer(layer.clone());
            (
                EmitResult::LayerAction(layer.clone()),
                Some(HeldAction::Layer(layer.clone())),
            )
        }
        KeyAction::TG(layer) => {
            // Toggles on press and latches: nothing is held, so release doesn't undo it
            if layer_stack.layers().contains(layer) {
                layer_stack.deactivate_layer(layer);
            } else {
                layer_stack.activate_layer(layer.clone());
            }
            (EmitResult::LayerAction(layer.clone()), None)
        }
        KeyAction::MO(layer) => {
            layer_stack.activate_layer(layer.clone());
            (
                EmitResult::LayerAction(layer.clone()),
                Some(HeldAction::Layer(layer.clone())),
            )
        }
        _ => (EmitResult::None, None),
    }
}

pub fn unemit_layer(
    action: &KeyAction,
    held_action: HeldAction,
    _keycode: KeyCode,
    layer_stack: &mut LayerStack,
) -> EmitResult {
    match (action, held_action) {
        (_, HeldAction::Layer(layer)) => {
            layer_stack.deactivate_layer(&layer);
            EmitResult::None
        }
        _ => EmitResult::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Layer};

    fn stack() -> LayerStack {
        let config: Config =
            ron::from_str(r#"(layers: { "steno": (kind: Remap, remaps: {}) })"#).unwrap();
        LayerStack::new(&config)
    }

    #[test]
    fn to_base_leaves_every_layer_that_was_switched_into() {
        let mut stack = stack();
        stack.activate_layer(Layer::new("steno"));
        let (result, held) = emit_layer(&KeyAction::TO(Layer::base()), KeyCode::KC_F10, &mut stack);
        assert!(matches!(result, EmitResult::LayerAction(_)));
        assert!(held.is_none());
        assert_eq!(stack.layers(), [Layer::base()]);
    }

    #[test]
    fn switch_turns_off_every_other_layer_and_stays_on() {
        let mut stack = stack();
        stack.activate_layer(Layer::new("steno"));
        let (result, held) =
            emit_layer(&KeyAction::SWITCH(Layer::new("nohrm")), KeyCode::KC_F9, &mut stack);
        assert!(matches!(result, EmitResult::LayerAction(_)));
        assert!(held.is_none(), "releasing F9 must not turn nohrm off");
        assert_eq!(stack.layers(), [Layer::base(), Layer::new("nohrm")]);
    }

    #[test]
    fn latch_turns_a_layer_on_and_nothing_holds_it() {
        let mut stack = stack();
        let (_, held) = emit_layer(&KeyAction::LATCH(Layer::new("steno")), KeyCode::KC_F1, &mut stack);
        assert!(held.is_none(), "releasing F1 must not turn steno off");
        assert_eq!(stack.current_layer(), Layer::new("steno"));
        // Pressing it again keeps it on, unlike TG
        emit_layer(&KeyAction::LATCH(Layer::new("steno")), KeyCode::KC_F1, &mut stack);
        assert_eq!(stack.current_layer(), Layer::new("steno"));
        assert_eq!(stack.layers(), [Layer::base(), Layer::new("steno")]);
    }
}
