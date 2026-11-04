//! How long each key is held when tapped, learned over time and kept between runs.

use crate::event_processor::actions::mt::RollingStats;
use crate::keycode::KeyCode;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// Taps shorter than this count as taps worth learning from
const TAP_THRESHOLD_MS: f32 = 130.0;
const TARGET_MARGIN_MS: f32 = 30.0;

pub struct AdaptiveProcessor {
    all_key_stats: HashMap<KeyCode, RollingStats>,
    key_press_times: HashMap<KeyCode, Instant>,
    /// Something was learned since the stats were last written to disk
    unsaved: bool,
}

impl AdaptiveProcessor {
    pub fn new() -> Self {
        Self {
            all_key_stats: HashMap::new(),
            key_press_times: HashMap::new(),
            unsaved: false,
        }
    }

    #[must_use]
    pub const fn has_unsaved_stats(&self) -> bool {
        self.unsaved
    }

    /// The key went down at `at`.
    pub fn record_key_press(&mut self, keycode: KeyCode, at: Instant) {
        self.key_press_times.insert(keycode, at);
    }

    /// The key came up at `at`: a tap's length is learned, outside game mode.
    pub fn record_key_release(&mut self, keycode: KeyCode, at: Instant, is_game_mode: bool) {
        let Some(press_time) = self.key_press_times.remove(&keycode) else {
            return;
        };
        let duration_ms = at.saturating_duration_since(press_time).as_millis() as f32;
        if duration_ms < TAP_THRESHOLD_MS && !is_game_mode {
            self.all_key_stats
                .entry(keycode)
                .or_insert_with(|| RollingStats::new(TAP_THRESHOLD_MS))
                .update_tap(duration_ms, TARGET_MARGIN_MS);
            self.unsaved = true;
        }
    }

    /// Write the stats to disk, if anything was learned since they were last written.
    pub fn save_adaptive_stats(&mut self, user_id: u32) -> Result<(), std::io::Error> {
        if !self.unsaved {
            return Ok(());
        }
        self.unsaved = false;
        let stats_map: HashMap<String, &RollingStats> = self
            .all_key_stats
            .iter()
            .map(|(keycode, stats)| (format!("{keycode:?}").replace("KC_", ""), stats))
            .collect();
        let json = serde_json::to_string_pretty(&stats_map)?;
        std::fs::write(Self::stats_path(user_id), json)?;
        tracing::info!("ALL KEYS: Saved {} key stats", self.all_key_stats.len());
        Ok(())
    }

    pub fn load_adaptive_stats(&mut self, user_id: u32) -> Result<(), std::io::Error> {
        let path = Self::stats_path(user_id);
        if !path.exists() {
            return Ok(());
        }
        let json = std::fs::read_to_string(path)?;
        let stats_map: HashMap<String, RollingStats> = serde_json::from_str(&json)?;
        self.all_key_stats.clear();
        for (key_str, stats) in stats_map {
            let key_json = format!("\"KC_{key_str}\"");
            if let Ok(keycode) = serde_json::from_str::<KeyCode>(&key_json) {
                self.all_key_stats.insert(keycode, stats);
            }
        }
        tracing::info!("ALL KEYS: Loaded {} key stats", self.all_key_stats.len());
        Ok(())
    }

    pub fn get_all_key_stats(&self) -> HashMap<KeyCode, RollingStats> {
        self.all_key_stats.clone()
    }

    fn stats_path(user_id: u32) -> PathBuf {
        crate::get_user_home_dir(user_id)
            .unwrap_or_else(|_| PathBuf::from("/root"))
            .join(".config/keymux/all_key_stats.json")
    }
}

impl Default for AdaptiveProcessor {
    fn default() -> Self {
        Self::new()
    }
}
