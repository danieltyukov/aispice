//! User configuration in `<config dir>/aispice/config.toml`.
//!
//! A missing file means defaults, and a file with only some fields set gets
//! defaults for the rest, so a fresh install needs no setup beyond a key.
//! Unknown fields are ignored so an older build can read a newer file. API
//! keys never go here; they live in [`crate::keys`].

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::DEFAULT_MAX_STEPS;
use crate::fsutil::write_atomic;
use crate::provider::{Effort, ThinkingConfig};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Provider id used when none is given.
    pub provider: String,
    /// Model for the default provider. `None` uses the provider's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Preferred simulator: `auto`, `ngspice`, `xyce`, `ltspice` or `spectre`.
    pub simulator: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ltspice_path: Option<PathBuf>,
    /// The Xyce executable, when it is not `Xyce` on PATH: PDK models that
    /// are Xyce plugins need a Xyce built with plugin support.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xyce_path: Option<PathBuf>,
    /// KLayout, for DRC, LVS, layout generation and pictures, when it is
    /// not `klayout` on PATH.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub klayout_path: Option<PathBuf>,
    /// Magic, for parasitic extraction, when it is not `magic` on PATH.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub magic_path: Option<PathBuf>,
    /// Simulate with aispice's embedded models and the project's own files
    /// only, never with models or symbols from an installed LTspice library,
    /// so results do not depend on the machine.
    #[serde(skip_serializing_if = "is_false")]
    pub embedded_models_only: bool,
    pub edit_mode: EditMode,
    pub agent: AgentSettings,
    /// Per-provider settings, keyed by provider id. A name that is not a
    /// built-in provider but has a `base_url` here is treated as an
    /// OpenAI-compatible endpoint (LM Studio, vLLM, a company gateway).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderSettings>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spectre: Option<SpectreSettings>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "anthropic".into(),
            model: None,
            simulator: "auto".into(),
            ltspice_path: None,
            xyce_path: None,
            klayout_path: None,
            magic_path: None,
            embedded_models_only: false,
            edit_mode: EditMode::Apply,
            agent: AgentSettings::default(),
            providers: BTreeMap::new(),
            spectre: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    pub max_steps: u32,
    /// Output limit per model call. `None` picks a per-provider default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    pub thinking: bool,
    /// Fixed thinking budget, only for models that still accept one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_budget: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            max_steps: DEFAULT_MAX_STEPS,
            max_tokens: None,
            thinking: true,
            thinking_budget: None,
            effort: None,
        }
    }
}

/// How to reach the Cadence Spectre host. These values come from the user,
/// never from the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpectreSettings {
    pub ssh_host: String,
    pub remote_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_command: Option<String>,
    /// Folders outside a project that the server has its own copy of, as
    /// `"local folder" = "folder on the server"`: an include of a file in
    /// one names the server's copy. Nothing outside the project is sent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub remote_folders: BTreeMap<String, String>,
}

/// Whether the agent's schematic edits apply at once (with undo) or wait for
/// approval.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EditMode {
    #[default]
    Apply,
    Ask,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not access {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("{path} is not valid: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("could not serialize the configuration: {0}")]
    Serialize(String),
    #[error("this system has no configuration directory")]
    NoConfigDir,
}

impl Config {
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("aispice").join("config.toml"))
    }

    /// Load from the default path, or defaults when there is no file.
    pub fn load() -> Result<Self, ConfigError> {
        match Self::default_path() {
            Some(path) => Self::load_from(&path),
            None => Ok(Self::default()),
        }
    }

    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(ConfigError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
    }

    /// Save to the default path. Returns the path written.
    pub fn save(&self) -> Result<PathBuf, ConfigError> {
        let path = Self::default_path().ok_or(ConfigError::NoConfigDir)?;
        self.save_to(&path)?;
        Ok(path)
    }

    /// Save atomically: write a temporary file next to `path`, then rename.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let text =
            toml::to_string_pretty(self).map_err(|e| ConfigError::Serialize(e.to_string()))?;
        write_atomic(path, text.as_bytes(), false).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// The model to use with `provider`: the configured one if `provider` is
    /// the default provider, else the provider's built-in default (if any).
    pub fn model_for(&self, provider: &str) -> Option<String> {
        let configured = (provider == self.provider)
            .then(|| self.model.clone())
            .flatten();
        configured.or_else(|| crate::providers::default_model(provider).map(str::to_string))
    }

    pub fn base_url_for(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .and_then(|p| p.base_url.as_deref())
            .filter(|u| !u.trim().is_empty())
    }

    /// The thinking request implied by the agent settings.
    pub fn thinking_config(&self) -> Option<ThinkingConfig> {
        self.agent.thinking.then_some(ThinkingConfig {
            budget_tokens: self.agent.thinking_budget,
            effort: self.agent.effort,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load_from(&dir.path().join("config.toml")).unwrap();
        assert_eq!(config, Config::default());
        assert_eq!(config.provider, "anthropic");
        assert_eq!(config.agent.max_steps, 40);
        assert_eq!(config.edit_mode, EditMode::Apply);
        assert_eq!(
            config.model_for("anthropic").as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(config.thinking_config(), Some(ThinkingConfig::adaptive()));
    }

    #[test]
    fn partial_file_fills_in_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
provider = "ollama"
model = "qwen3:8b"
edit_mode = "ask"
future_option = true

[agent]
effort = "xhigh"

[providers.ollama]
base_url = "http://gpu-box:11434/v1"

[spectre]
ssh_host = "eda.example.edu"
remote_dir = "/scratch/me/aispice"
"#,
        )
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.provider, "ollama");
        assert_eq!(config.model_for("ollama").as_deref(), Some("qwen3:8b"));
        assert_eq!(
            config.model_for("anthropic").as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(config.model_for("openai"), None);
        assert_eq!(config.edit_mode, EditMode::Ask);
        assert_eq!(config.agent.max_steps, 40);
        assert_eq!(config.agent.effort, Some(Effort::XHigh));
        assert_eq!(
            config.base_url_for("ollama"),
            Some("http://gpu-box:11434/v1")
        );
        assert_eq!(config.base_url_for("openai"), None);
        let spectre = config.spectre.unwrap();
        assert_eq!(spectre.ssh_host, "eda.example.edu");
        assert_eq!(spectre.setup_command, None);
    }

    #[test]
    fn save_and_load_round_trip_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aispice").join("config.toml");
        let mut config = Config {
            provider: "openrouter".into(),
            model: Some("anthropic/claude-opus-5-5".into()),
            simulator: "ngspice".into(),
            ltspice_path: Some(PathBuf::from("/opt/ltspice/LTspice.exe")),
            edit_mode: EditMode::Ask,
            ..Config::default()
        };
        config.agent.max_steps = 12;
        config.agent.thinking_budget = Some(4096);
        config.providers.insert(
            "lmstudio".into(),
            ProviderSettings {
                base_url: Some("http://localhost:1234/v1".into()),
            },
        );
        config.spectre = Some(SpectreSettings {
            ssh_host: "eda".into(),
            remote_dir: "/tmp/x".into(),
            setup_command: Some("source /cad/setup.sh".into()),
            remote_folders: BTreeMap::from([("/home/me/pdk".into(), "/cad/pdk".into())]),
        });
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap(), config);

        config.agent.max_steps = 20;
        config.save_to(&path).unwrap();
        assert_eq!(Config::load_from(&path).unwrap().agent.max_steps, 20);
        let entries: Vec<_> = std::fs::read_dir(path.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1, "temporary file left behind");
        assert_eq!(
            config.thinking_config(),
            Some(ThinkingConfig::with_budget(4096))
        );
    }

    #[test]
    fn embedded_models_only_is_read_and_off_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "embedded_models_only = true\n").unwrap();
        assert!(Config::load_from(&path).unwrap().embedded_models_only);
        let default = Config::default();
        assert!(!default.embedded_models_only);
        // Left out of a saved file while it is off.
        default.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("embedded_models_only"), "{text}");
    }

    #[test]
    fn thinking_can_be_turned_off() {
        let mut config = Config::default();
        config.agent.thinking = false;
        assert_eq!(config.thinking_config(), None);
    }

    #[test]
    fn invalid_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "provider = [unclosed").unwrap();
        let err = Config::load_from(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
        assert!(err.to_string().contains("config.toml"));
    }
}
