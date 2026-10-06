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
