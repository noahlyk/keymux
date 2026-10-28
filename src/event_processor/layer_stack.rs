use crate::config::{Config, KeyAction, Layer, LayerConfig, DEFAULT_LAYER_PRIORITY};
use crate::keycode::KeyCode;
use std::collections::HashMap;
use tracing::warn;

#[derive(Debug, Clone)]
pub struct LayerStack {
    layers: Vec<Layer>,
    layer_configs: HashMap<Layer, LayerConfig>,
    base_remaps: HashMap<KeyCode, KeyAction>,
    game_mode_active: bool,
    game_mode_remaps: HashMap<KeyCode, KeyAction>,
}

impl LayerStack {
    pub fn new(config: &Config) -> Self {
        let mut layer_configs = HashMap::new();
        for (layer, layer_config) in &config.layers {
            layer_configs.insert(layer.clone(), layer_config.clone());
        }

        // The base layer, then the default layer on top of it if one is set. A default layer
        // that isn't defined is ignored, so a typo can't leave the keyboard with no layer to type on.
        let mut stack = Self {
            layers: vec![Layer::base()],
            layer_configs,
            base_remaps: config.remaps.clone(),
            game_mode_active: false,
            game_mode_remaps: config.game_mode.remaps.clone(),
        };
        if let Some(layer) = &config.default_layer {
            if stack.layer_configs.contains_key(layer) {
                stack.activate_layer(layer.clone());
            } else {
                warn!(
                    "default_layer \"{}\" is not defined in layers; using base",
                    layer.0
                );
            }
        }
        stack
    }

    pub fn current_layer(&self) -> Layer {
        self.layers.last().cloned().unwrap_or_else(Layer::base)
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    /// How high a layer sits: the base layer is 0, a layer says so with `priority`, and a layer
    /// the config doesn't define gets the default.
    fn priority(&self, layer: &Layer) -> u32 {
        if layer.is_base() {
            return 0;
        }
        self.layer_configs
            .get(layer)
            .map_or(DEFAULT_LAYER_PRIORITY, |config| config.priority)
    }

    /// Turn a layer on. It goes above every layer with a lower or equal priority, so of the
    /// layers with the same priority the newest is on top. A layer that is already on is raised
    /// the same way, as if it had just been turned on.
    pub fn activate_layer(&mut self, layer: Layer) {
        if layer.is_base() {
            return;
        }
        self.layers.retain(|on| on != &layer);
        let priority = self.priority(&layer);
        // The stack is in order of priority, so the layers that stay below are a prefix of it
        let above = self
            .layers
            .iter()
            .rposition(|on| self.priority(on) <= priority)
            .map_or(0, |last| last + 1);
        self.layers.insert(above, layer);
    }

    /// Turn every layer off, back to the base layer. Used by `TO("base")`, the way to leave
    /// a layer that was switched into, such as steno.
    pub fn leave_to_base(&mut self) {
        self.layers = vec![Layer::base()];
    }

    pub fn deactivate_layer(&mut self, layer: &Layer) {
        if !layer.is_base() {
            self.layers.retain(|l| l != layer);
        }
    }

    pub fn toggle_layer(&mut self, layer: Layer) {
        if self.layers.contains(&layer) {
            self.deactivate_layer(&layer);
        } else {
            self.activate_layer(layer);
        }
    }

    pub const fn set_game_mode(&mut self, active: bool) {
        self.game_mode_active = active;
    }

    pub const fn is_game_mode_active(&self) -> bool {
        self.game_mode_active
    }

    pub const fn base_remaps(&self) -> &HashMap<KeyCode, KeyAction> {
        &self.base_remaps
    }

    pub const fn game_mode_remaps(&self) -> &HashMap<KeyCode, KeyAction> {
        &self.game_mode_remaps
    }

    pub const fn layer_configs(&self) -> &HashMap<Layer, LayerConfig> {
        &self.layer_configs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> Config {
        ron::from_str(&format!(
            r#"(
                layers: {{ "steno": (kind: Remap, remaps: {{}}) }},
                {extra}
            )"#
        ))
        .unwrap()
    }

    #[test]
    fn starts_on_the_base_layer_without_a_default_layer() {
        let stack = LayerStack::new(&config(""));
        assert_eq!(stack.layers(), [Layer::base()]);
    }

    #[test]
    fn a_default_layer_is_on_from_the_start() {
        let stack = LayerStack::new(&config(r#"default_layer: Some("steno"),"#));
        assert_eq!(stack.layers(), [Layer::base(), Layer::new("steno")]);
        assert_eq!(stack.current_layer(), Layer::new("steno"));
    }

    fn priorities(layers: &[(&str, u32)]) -> LayerStack {
        let defs: Vec<String> = layers
            .iter()
            .map(|(name, priority)| {
                format!(r#""{name}": (kind: Remap, remaps: {{}}, priority: {priority}),"#)
            })
            .collect();
        LayerStack::new(&ron::from_str(&format!("(layers: {{ {} }})", defs.join(" "))).unwrap())
    }

    fn names(stack: &LayerStack) -> Vec<String> {
        stack.layers().iter().map(|layer| layer.0.clone()).collect()
    }

    #[test]
    fn layers_that_do_not_say_have_priority_1_and_the_newest_is_on_top() {
        let mut stack = LayerStack::new(
            &ron::from_str(r#"(layers: { "a": (kind: Remap), "b": (kind: Remap) })"#).unwrap(),
        );
        stack.activate_layer(Layer::new("a"));
        stack.activate_layer(Layer::new("b"));
        assert_eq!(names(&stack), ["base", "a", "b"]);
        assert_eq!(stack.current_layer(), Layer::new("b"));
    }

    #[test]
    fn a_higher_priority_layer_stays_on_top_whichever_was_turned_on_first() {
        let mut stack = priorities(&[("low", 1), ("high", 5)]);
        stack.activate_layer(Layer::new("high"));
        stack.activate_layer(Layer::new("low"));
        assert_eq!(names(&stack), ["base", "low", "high"]);
        assert_eq!(stack.current_layer(), Layer::new("high"));
    }

    #[test]
    fn turning_a_layer_off_keeps_the_order_of_the_rest() {
        let mut stack = priorities(&[("a", 1), ("b", 3), ("c", 2)]);
        for name in ["c", "b", "a"] {
            stack.activate_layer(Layer::new(name));
        }
        assert_eq!(names(&stack), ["base", "a", "c", "b"]);
        stack.deactivate_layer(&Layer::new("c"));
        assert_eq!(names(&stack), ["base", "a", "b"]);
    }

    #[test]
    fn layers_with_the_same_priority_stack_in_the_order_they_were_turned_on() {
        let mut stack = priorities(&[("a", 4), ("b", 4), ("c", 4)]);
        for name in ["b", "c", "a"] {
            stack.activate_layer(Layer::new(name));
        }
        assert_eq!(names(&stack), ["base", "b", "c", "a"]);
    }

    #[test]
    fn turning_on_a_layer_that_is_already_on_raises_it_among_layers_of_its_priority() {
        let mut stack = priorities(&[("a", 1), ("b", 1), ("top", 9)]);
        for name in ["a", "b", "top"] {
            stack.activate_layer(Layer::new(name));
        }
        stack.activate_layer(Layer::new("a"));
        // Raised above b, but not above a layer with a higher priority
        assert_eq!(names(&stack), ["base", "b", "a", "top"]);
        // And it is only in the stack once
        stack.deactivate_layer(&Layer::new("a"));
        assert_eq!(names(&stack), ["base", "b", "top"]);
    }

    #[test]
    fn a_layer_with_priority_0_is_above_base_but_below_the_default() {
        let mut stack = priorities(&[("zero", 0), ("one", 1)]);
        stack.activate_layer(Layer::new("one"));
        stack.activate_layer(Layer::new("zero"));
        assert_eq!(names(&stack), ["base", "zero", "one"]);
    }

    #[test]
    fn the_base_layer_is_always_at_the_bottom_and_cannot_be_raised() {
        let mut stack = priorities(&[("a", 0)]);
        stack.activate_layer(Layer::new("a"));
        stack.activate_layer(Layer::base());
        assert_eq!(names(&stack), ["base", "a"]);
    }

    #[test]
    fn a_default_layer_takes_its_place_by_priority() {
        let mut stack = LayerStack::new(
            &ron::from_str(
                r#"(
                    default_layer: Some("steno"),
                    layers: {
                        "steno": (kind: Remap, remaps: {}, priority: 2),
                        "nav": (kind: Remap, remaps: {}, priority: 3),
                    },
                )"#,
            )
            .unwrap(),
        );
        assert_eq!(names(&stack), ["base", "steno"]);
        stack.activate_layer(Layer::new("nav"));
        assert_eq!(names(&stack), ["base", "steno", "nav"]);
    }

    #[test]
    fn an_undefined_default_layer_is_ignored() {
        let stack = LayerStack::new(&config(r#"default_layer: Some("nope"),"#));
        assert_eq!(stack.layers(), [Layer::base()]);
    }
}
