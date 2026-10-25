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
}
