//! What the on-screen overlay is told: which steno layer this keyboard has up, if any.

use super::KeymapProcessor;

impl KeymapProcessor {
    /// The steno layer that is on top for this keyboard, if any: what the overlay should show.
    pub(super) fn overlay_layer_name(&self) -> Option<&str> {
        // An open lookup keeps the overlay on its layer while a layer is held above it
        let layers = self.layer_stack.layers();
        if let Some((layer, _)) = self
            .steno_engines
            .iter()
            .find(|(layer, engine)| engine.lookup_active() && layers.contains(layer))
        {
            return Some(layer.0.as_str());
        }
        let game_mode = self.layer_stack.is_game_mode_active();
        layers
            .last()
            .filter(|layer| {
                self.steno_engines
                    .get(*layer)
                    .is_some_and(|engine| !(game_mode && engine.disabled_in_game_mode))
            })
            .map(|layer| layer.0.as_str())
    }

    /// Tell the overlay about this keyboard's steno state. It runs on every key event, so
    /// whichever keyboard was touched last decides what the overlay shows: a steno keyboard
    /// shows it, and touching one that isn't in steno hides it. This also shows it on the
    /// press that switches into steno, not on the next stroke. The hub ignores a repeat of
    /// what it already has, so the common case is one uncontended lock and a comparison.
    pub(super) fn publish_overlay_layer(&self) {
        self.overlay_hub.set_layer(self.overlay_layer_name());
    }

    /// Publish to `hub` instead of the daemon's, from here on. Tests use this so they
    /// don't share state through the global hub.
    #[cfg(test)]
    #[cfg(test)]
    pub(crate) fn set_overlay_hub(&mut self, hub: &'static crate::steno::overlay::OverlayHub) {
        self.overlay_hub = hub;
        for engine in self.steno_engines.values_mut() {
            engine.set_hub(hub);
        }
        if self.overlay_layer_name().is_some() {
            self.publish_overlay_layer();
        }
    }
}
