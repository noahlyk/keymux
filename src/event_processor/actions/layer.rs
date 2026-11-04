use crate::config::KeyAction;
use crate::event_processor::actions::{EmitResult, HeldAction};
use crate::event_processor::layer_stack::LayerStack;

/// A layer switch types nothing; what it leaves held, if anything, is the layer to let go
/// of on release.
pub fn emit_layer(action: &KeyAction, layer_stack: &mut LayerStack) -> Option<HeldAction> {
    match action {
        // Exclusive, like QMK's TO: every other layer goes, and this one stays on after release.
        // TO("base") is the way back to base, since base is then the only layer left.
        KeyAction::TO(layer) => {
            layer_stack.leave_to_base();
            layer_stack.activate_layer(layer.clone());
            None
        }
        // Momentary, like QMK's MO: on while the key is held, off when it is released
        KeyAction::MO(layer) => {
            layer_stack.activate_layer(layer.clone());
            Some(HeldAction::Layer(layer.clone()))
        }
        // Toggles on press and latches: nothing is held, so release doesn't undo it
        KeyAction::TG(layer) => {
            layer_stack.toggle_layer(layer.clone());
            None
        }
        _ => None,
    }
}

/// Lets go of what a layer key held: the layer comes off.
pub fn unemit_layer(held: &HeldAction, layer_stack: &mut LayerStack) -> EmitResult {
    if let HeldAction::Layer(layer) = held {
        layer_stack.deactivate_layer(layer);
    }
    EmitResult::None
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
        let held = emit_layer(&KeyAction::TO(Layer::base()), &mut stack);
        assert!(held.is_none());
        assert_eq!(stack.layers(), [Layer::base()]);
    }

    #[test]
    fn to_turns_off_every_other_layer_and_stays_on() {
        let mut stack = stack();
        stack.activate_layer(Layer::new("steno"));
        let held = emit_layer(&KeyAction::TO(Layer::new("nohrm")), &mut stack);
        assert!(held.is_none(), "releasing F9 must not turn nohrm off");
        assert_eq!(stack.layers(), [Layer::base(), Layer::new("nohrm")]);
    }

    #[test]
    fn mo_holds_a_layer_until_it_is_released() {
        let mut stack = stack();
        let held = emit_layer(&KeyAction::MO(Layer::new("nav")), &mut stack);
        assert_eq!(stack.current_layer(), Layer::new("nav"));
        let held = held.expect("MO must hand back something to release");
        unemit_layer(&held, &mut stack);
        assert_eq!(stack.layers(), [Layer::base()]);
    }

    #[test]
    fn tg_latches_and_a_second_tap_turns_it_off() {
        let mut stack = stack();
        assert!(emit_layer(&KeyAction::TG(Layer::new("steno")), &mut stack).is_none());
        assert_eq!(stack.current_layer(), Layer::new("steno"));
        emit_layer(&KeyAction::TG(Layer::new("steno")), &mut stack);
        assert_eq!(stack.layers(), [Layer::base()]);
    }
}
