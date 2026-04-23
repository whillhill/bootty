use anyhow::{bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const CONFIG_DIR_NAME: &str = ".bootty";
const CONFIG_FILE_NAME: &str = "config.json";
const RUN_DIR_NAME: &str = "run";
const SERVE_RUNTIME_FILE_NAME: &str = "serve.json";
const CONFIG_VERSION: u32 = 1;
const DEFAULT_MAX_SESSIONS: usize = 128;
const DEFAULT_PORT: u16 = 2234;
const DEFAULT_STUN_SERVER: &str = "stun:stun.l.google.com:19302";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ServeMode {
    Local,
    LanOpen,
    LanAuth,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ServeAuthType {
    None,
    Pin,
    Password,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoottyConfig {
    pub version: u32,
    pub network: NetworkConfig,
    pub host: HostConfig,
    pub serve: ServeConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub stun_servers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostConfig {
    pub default_cmd: Vec<String>,
    pub non_interactive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeConfig {
    pub host: String,
    pub port: u16,
    pub max_sessions: usize,
    pub mode: ServeMode,
    pub auth: ServeAuthConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeAuthConfig {
    #[serde(rename = "type")]
    pub auth_type: ServeAuthType,
    pub password_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServeRuntimeInfo {
    pub pid: u32,
    pub admin_addr: String,
    pub admin_token: String,
    pub started_at_unix: u64,
}

impl Default for BoottyConfig {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            network: NetworkConfig {
                stun_servers: vec![DEFAULT_STUN_SERVER.to_string()],
            },
            host: HostConfig {
                default_cmd: vec!["bash".to_string(), "-l".to_string()],
                non_interactive: false,
            },
            serve: ServeConfig {
                host: "127.0.0.1".to_string(),
                port: DEFAULT_PORT,
                max_sessions: DEFAULT_MAX_SESSIONS,
                mode: ServeMode::Local,
                auth: ServeAuthConfig {
                    auth_type: ServeAuthType::None,
                    password_hash: None,
                },
            },
        }
    }
}

pub fn load_or_create_config() -> Result<BoottyConfig> {
    ensure_config_dir()?;
    let path = config_path()?;

    if !path.exists() {
        let default = BoottyConfig::default();
        save_config(&default)?;
        return Ok(default);
    }

    let text = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    let cfg: BoottyConfig = serde_json::from_str(&text).with_context(|| {
        format!(
            "Failed to parse config file: {}. {}",
            path.display(),
            config_recovery_hint()
        )
    })?;
    validate_config(&cfg).with_context(|| {
        format!(
            "Invalid config file: {}. {}",
            path.display(),
            config_recovery_hint()
        )
    })?;
    Ok(cfg)
}

pub fn save_config(cfg: &BoottyConfig) -> Result<()> {
    ensure_config_dir()?;
    validate_config(cfg)?;
    let path = config_path()?;
    let text = serde_json::to_string_pretty(cfg).context("Failed to serialize config")?;
    fs::write(&path, format!("{text}\n"))
        .with_context(|| format!("Failed to write config file: {}", path.display()))?;
    Ok(())
}

pub fn init_config(force: bool) -> Result<BoottyConfig> {
    let path = config_path()?;
    if path.exists() && !force {
        bail!("Config file already exists, use --force to overwrite");
    }
    let cfg = BoottyConfig::default();
    save_config(&cfg)?;
    Ok(cfg)
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join(CONFIG_FILE_NAME))
}

pub fn serve_runtime_path() -> Result<PathBuf> {
    Ok(run_dir()?.join(SERVE_RUNTIME_FILE_NAME))
}

pub fn write_serve_runtime(info: &ServeRuntimeInfo) -> Result<()> {
    ensure_run_dir()?;
    let path = serve_runtime_path()?;
    let text = serde_json::to_string_pretty(info).context("Failed to serialize runtime file")?;
    fs::write(&path, format!("{text}\n"))
        .with_context(|| format!("Failed to write runtime file: {}", path.display()))?;
    Ok(())
}

pub fn read_serve_runtime() -> Result<ServeRuntimeInfo> {
    let path = serve_runtime_path()?;
    let text = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read runtime file: {}", path.display()))?;
    let info: ServeRuntimeInfo =
        serde_json::from_str(&text).with_context(|| format!("Failed to parse runtime file: {}", path.display()))?;
    Ok(info)
}

pub fn remove_serve_runtime() -> Result<()> {
    let path = serve_runtime_path()?;
    if !path.exists() {
        return Ok(());
    }
    fs::remove_file(&path).with_context(|| format!("Failed to remove runtime file: {}", path.display()))?;
    Ok(())
}

pub fn hash_password(password: &str) -> Result<String> {
    if password.is_empty() {
        bail!("Password cannot be empty");
    }

    let mut salt = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut salt);
    let salt_hex = hex::encode(salt);
    let digest = sha256_hex_with_salt(&salt_hex, password);
    Ok(format!("sha256:{salt_hex}:{digest}"))
}

pub fn verify_password(password: &str, encoded: &str) -> Result<bool> {
    let parts: Vec<&str> = encoded.split(':').collect();
    if parts.len() != 3 {
        bail!("Invalid password hash format");
    }
    if parts[0] != "sha256" {
        bail!("Unsupported password hash algorithm: {}", parts[0]);
    }
    let salt_hex = parts[1];
    let expected = parts[2];
    let digest = sha256_hex_with_salt(salt_hex, password);
    Ok(digest == expected)
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sha256_hex_with_salt(salt_hex: &str, password: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt_hex.as_bytes());
    hasher.update(b":");
    hasher.update(password.as_bytes());
    let output = hasher.finalize();
    hex::encode(output)
}

fn config_recovery_hint() -> &'static str {
    "Hint: run `bootty config init --force` to regenerate config, or run `bootty config path` to locate the file."
}

fn validate_config(cfg: &BoottyConfig) -> Result<()> {
    if cfg.version != CONFIG_VERSION {
        bail!(
            "Unsupported config version: {}, expected {}",
            cfg.version,
            CONFIG_VERSION
        );
    }
    if cfg.network.stun_servers.is_empty() {
        bail!("Invalid config: network.stun_servers cannot be empty");
    }
    if cfg.host.default_cmd.is_empty() {
        bail!("Invalid config: host.default_cmd cannot be empty");
    }
    if cfg.serve.max_sessions == 0 {
        bail!("Invalid config: serve.max_sessions must be greater than 0");
    }
    if cfg.serve.mode == ServeMode::LanAuth && cfg.serve.auth.auth_type == ServeAuthType::None {
        bail!("Invalid config: serve.auth.type cannot be none when serve.mode=lan-auth");
    }
    if cfg.serve.auth.auth_type == ServeAuthType::Password && cfg.serve.auth.password_hash.is_none() {
        bail!("Invalid config: serve.auth.password_hash is required when serve.auth.type=password");
    }
    Ok(())
}

fn config_dir() -> Result<PathBuf> {
    let home = detect_home_dir()?;
    Ok(home.join(CONFIG_DIR_NAME))
}

fn run_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join(RUN_DIR_NAME))
}

fn ensure_config_dir() -> Result<()> {
    let dir = config_dir()?;
    if !dir.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("Failed to create config directory: {}", dir.display()))?;
    }
    Ok(())
}

fn ensure_run_dir() -> Result<()> {
    ensure_config_dir()?;
    let dir = run_dir()?;
    if !dir.exists() {
        fs::create_dir_all(&dir).with_context(|| format!("Failed to create runtime directory: {}", dir.display()))?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{hash_password, verify_password};

    #[test]
    fn password_hash_roundtrip() {
        let encoded = hash_password("abc123").expect("hash password");
        assert!(verify_password("abc123", &encoded).expect("verify password"));
        assert!(!verify_password("abc456", &encoded).expect("verify password mismatch"));
    }
}
