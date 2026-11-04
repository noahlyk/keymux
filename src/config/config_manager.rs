/// Config Manager - Smart hot-reload capabilities
///
/// Handles configuration loading for multi-user daemon.
use crate::config::Config;
use anyhow::{Context, Result};

use std::path::PathBuf;
use std::sync::Arc;

/// A user's loaded configuration. A reload makes a new manager; a loaded config never changes.
#[derive(Clone)]
pub struct ConfigManager {
    config: Arc<Config>,
    /// Path to the config file
    config_path: PathBuf,
}

impl ConfigManager {
    /// Create a new config manager
    pub fn new(config_path: PathBuf) -> Result<Self> {
        let config = Config::load(&config_path)
            .with_context(|| format!("Failed to load config from {}", config_path.display()))?;

        Ok(Self {
            config: Arc::new(config),
            config_path,
        })
    }

    /// Get the current configuration
    pub fn get_config(&self) -> Config {
        (*self.config).clone()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Get the config file path
    pub fn get_config_path(&self) -> PathBuf {
        self.config_path.clone()
    }
}
