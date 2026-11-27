//! Persistence for "Attached Accounts" in
//! `~/Library/Application Support/azure-storage-tui/config.json`.
//! The legacy path remains unchanged in az-storage to keep existing accounts
//! accessible, without migrations or format changes.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub attached_accounts: Vec<AttachedAccount>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub nerd_font_icons: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachedAccount {
    pub name: String,
    pub connection_string: String,
}

pub fn path() -> PathBuf {
    // On macOS, `data_dir()` == ~/Library/Application Support
    let base = directories::BaseDirs::new()
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("azure-storage-tui").join("config.json")
}

pub fn load() -> Result<Config> {
    let p = path();
    if !p.exists() {
        return Ok(Config::default());
    }
    let raw = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    if raw.trim().is_empty() {
        return Ok(Config::default());
    }
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))
}

pub fn save(cfg: &Config) -> Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let raw = serde_json::to_string_pretty(cfg)?;
    let tmp = p.with_extension("json.tmp");
    std::fs::write(&tmp, raw)?;
    std::fs::rename(&tmp, &p)?;
    #[cfg(unix)]
    {
        // The file contains secrets (AccountKey / SAS): owner access only.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}
