use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::path::PathBuf;

const CONFIG_DIR_NAME: &str = ".bootty";
const CONFIG_FILE_NAME: &str = "config.json";
const DEFAULT_MAX_SESSIONS: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoottyConfig {
    pub max_sessions: usize,
}

impl Default for BoottyConfig {
    fn default() -> Self {
        Self {
            max_sessions: DEFAULT_MAX_SESSIONS,
        }
    }
}

pub fn load_or_create_config() -> Result<BoottyConfig> {
    let home = detect_home_dir()?;
    let config_dir = home.join(CONFIG_DIR_NAME);
    let config_path = config_dir.join(CONFIG_FILE_NAME);

    if !config_dir.exists() {
        fs::create_dir_all(&config_dir)
            .with_context(|| format!("Failed to create config directory: {}", config_dir.display()))?;
    }

    if !config_path.exists() {
        let default = BoottyConfig::default();
        let text = serde_json::to_string_pretty(&default).context("Failed to serialize default config")?;
        fs::write(&config_path, format!("{text}\n"))
            .with_context(|| format!("Failed to write default config: {}", config_path.display()))?;
        return Ok(default);
    }

    let text = fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read config: {}", config_path.display()))?;
    let cfg: BoottyConfig = serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse config: {}", config_path.display()))?;

    if cfg.max_sessions == 0 {
        bail!(
            "Invalid config: max_sessions must be greater than 0 (file: {})",
            config_path.display()
        );
    }

    Ok(cfg)
}

fn detect_home_dir() -> Result<PathBuf> {
    if let Some(home) = env::var_os("HOME") {
        return Ok(PathBuf::from(home));
    }
    if let Some(home) = env::var_os("USERPROFILE") {
        return Ok(PathBuf::from(home));
    }
    bail!("Unable to determine home directory from HOME/USERPROFILE")
}
