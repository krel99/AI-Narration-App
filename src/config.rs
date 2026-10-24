//! App-level settings (API keys, library location), stored as JSON in the
//! XDG config directory with owner-only permissions.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};

use crate::worker::ApiKeys;

/// Where the Claude passes (speaker attribution, audio tags) run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaudeBackend {
    /// The user's own Claude Code CLI, signed in with their Pro/Max plan.
    /// The default: the user asked for the app to use Claude Code.
    #[default]
    ClaudeCode,
    /// The Anthropic API with an API key, billed per token.
    ApiKey,
}

impl<'de> Deserialize<'de> for ClaudeBackend {
    /// Unknown values fall back to the default (Claude Code) instead of
    /// failing the whole config file (which would lose the stored keys).
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        Ok(match v.as_str() {
            Some("claude_code") => ClaudeBackend::ClaudeCode,
            Some("api_key") => ClaudeBackend::ApiKey,
            _ => ClaudeBackend::default(),
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub anthropic_api_key: String,
    pub elevenlabs_api_key: String,
    /// Overrides the default library directory (takes effect on restart).
    pub library_dir: Option<PathBuf>,
    pub last_project: Option<i64>,
    /// Dark theme instead of the default light one.
    pub dark_mode: bool,
    /// Claude Code (subscription) or the Anthropic API (key).
    pub claude_backend: ClaudeBackend,
    /// The `claude` binary; found on PATH or in ~/.local/bin when unset.
    pub claude_code_path: Option<PathBuf>,
    /// What 1,000 ElevenLabs credits cost the user, to show money next to
    /// credit estimates. None hides it.
    pub price_per_1000_credits: Option<f64>,
    /// Currency label for `price_per_1000_credits` ("USD", "€").
    pub credit_currency: String,
    /// Local take check (Whisper): machine-dependent, so app-level.
    pub check: crate::check::CheckConfig,
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
            claude_backend: self.claude_backend,
            claude_code_path: self
                .claude_code_path
                .clone()
                .filter(|p| !p.as_os_str().is_empty()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_configs_load_and_backends_roundtrip() {
        let old: AppConfig =
            serde_json::from_str(r#"{"anthropic_api_key":"k","dark_mode":true}"#).unwrap();
        // A config without the field (or with an unknown value) uses Claude Code.
        assert_eq!(old.claude_backend, ClaudeBackend::ClaudeCode);
        let api: AppConfig = serde_json::from_str(r#"{"claude_backend":"api_key"}"#).unwrap();
        assert_eq!(api.claude_backend, ClaudeBackend::ApiKey);
        assert_eq!(old.anthropic_api_key, "k");
        assert_eq!(old.price_per_1000_credits, None);

        let cfg = AppConfig {
            claude_backend: ClaudeBackend::ClaudeCode,
            claude_code_path: Some("/opt/claude".into()),
            price_per_1000_credits: Some(0.24),
            credit_currency: "USD".into(),
            ..Default::default()
        };
        let back: AppConfig = serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        let keys = back.api_keys();
        assert_eq!(keys.claude_backend, ClaudeBackend::ClaudeCode);
        assert_eq!(keys.claude_code_path, Some(PathBuf::from("/opt/claude")));

        // A value from a newer version does not lose the rest of the file.
        let odd: AppConfig =
            serde_json::from_str(r#"{"elevenlabs_api_key":"e","claude_backend":"something_new"}"#)
                .unwrap();
        assert_eq!(
            (odd.elevenlabs_api_key.as_str(), odd.claude_backend),
            ("e", ClaudeBackend::ClaudeCode)
        );
        let odd: AppConfig =
            serde_json::from_str(r#"{"elevenlabs_api_key":"e","claude_backend":5}"#).unwrap();
        assert_eq!(odd.elevenlabs_api_key, "e");
    }
}
