//! App-level settings (API keys, library location), stored as JSON in the
//! XDG config directory with owner-only permissions.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::worker::ApiKeys;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub anthropic_api_key: String,
    pub elevenlabs_api_key: String,
    /// Overrides the default library directory (takes effect on restart).
    pub library_dir: Option<PathBuf>,
    pub last_project: Option<i64>,
}

fn dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "narration-studio")
}

impl AppConfig {
    pub fn path() -> Option<PathBuf> {
        dirs().map(|d| d.config_dir().join("config.json"))
    }

    pub fn load() -> Self {
        Self::path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn library_dir(&self) -> PathBuf {
        self.library_dir
            .clone()
            .or_else(|| dirs().map(|d| d.data_dir().to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("narration-studio-library"))
    }

    /// Keys from the config file, falling back to the environment.
    pub fn api_keys(&self) -> ApiKeys {
        let pick = |stored: &str, var: &str| {
            if stored.trim().is_empty() {
                std::env::var(var).unwrap_or_default()
            } else {
                stored.trim().to_string()
            }
        };
        ApiKeys {
            anthropic: pick(&self.anthropic_api_key, "ANTHROPIC_API_KEY"),
            elevenlabs: pick(&self.elevenlabs_api_key, "ELEVENLABS_API_KEY"),
        }
    }
}
