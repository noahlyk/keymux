use crate::config::{Config, KeyAction, Layer, LayerConfig};
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
        let mut layers = vec![Layer::base()];
        if let Some(layer) = &config.default_layer {
            if layer_configs.contains_key(layer) {
                layers.push(layer.clone());
            } else {
                warn!("default_layer \"{}\" is not defined in layers; using base", layer.0);
            }
        }

        Self {
            layers,
            layer_configs,
            base_remaps: config.remaps.clone(),
            game_mode_active: false,
            game_mode_remaps: config.game_mode.remaps.clone(),
        }
    }

    pub fn current_layer(&self) -> Layer {
        self.layers.last().cloned().unwrap_or_else(Layer::base)
    }

    pub fn layers(&self) -> &[Layer] {
        &self.layers
    }

    pub fn activate_layer(&mut self, layer: Layer) {
        if !self.layers.contains(&layer) {
            self.layers.push(layer);
        }
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

    #[test]
    fn an_undefined_default_layer_is_ignored() {
        let stack = LayerStack::new(&config(r#"default_layer: Some("nope"),"#));
        assert_eq!(stack.layers(), [Layer::base()]);
    }
}
