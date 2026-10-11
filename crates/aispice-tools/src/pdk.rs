//! Registered PDKs, and a project's choice of one.
//!
//! The registry is a user file, `<config dir>/aispice/pdks.toml`, that only
//! the user changes (`aispice pdk add`): registering a folder makes aispice
//! load compiled code from it, so neither a project nor the agent can do it.
//! It is kept apart from `config.toml` because the desktop app writes that
//! file back from memory and would drop an entry added while it is open.
//!
//! A project opts in with `pdk = "<registered name>"` in `aispice.toml` at
//! its root. That file is part of the project and so untrusted: it can name a
//! registered PDK and nothing else, never a path.

use aispice_sim::pdk::{Pdk, PdkError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The registry's file name, beside `config.toml`.
pub const REGISTRY_FILE: &str = "pdks.toml";
/// A project's own settings file, at its root.
pub const PROJECT_FILE: &str = "aispice.toml";
/// `aispice.toml` larger than this is refused rather than read.
const MAX_PROJECT_FILE: u64 = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error(
        "`{0}` is not a usable PDK name: use letters, digits, `.`, `_` and `-`, starting with a letter or digit, at most 64 characters"
    )]
    BadName(String),
    #[error("no PDK is registered as `{name}`{}", known_list(.known))]
    Unknown { name: String, known: Vec<String> },
    #[error(transparent)]
    Pdk(#[from] PdkError),
    #[error("{} is not valid: {message}", .path.display())]
    Parse { path: PathBuf, message: String },
    #[error("could not read or write {}: {message}", .path.display())]
    Io { path: PathBuf, message: String },
    #[error("this system has no configuration folder")]
    NoConfigDir,
}

fn known_list(known: &[String]) -> String {
    if known.is_empty() {
        "; none are registered (run `aispice pdk add <name> <path>`)".into()
    } else {
        format!("; registered: {}", known.join(", "))
    }
}

/// One registered PDK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryEntry {
    /// The PDK folder, as checked when it was added.
    pub path: PathBuf,
}

/// The user's registered PDKs, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub pdks: BTreeMap<String, RegistryEntry>,
}

/// Whether a name can be registered: it becomes a folder name in the gm/Id
/// cache, so nothing that could climb out of it.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Registry {
    /// `<config dir>/aispice/pdks.toml`.
    pub fn default_path() -> Option<PathBuf> {
        aispice_agent::Config::default_path()
            .and_then(|p| p.parent().map(|d| d.join(REGISTRY_FILE)))
    }

    /// The user's registry; empty when there is no file.
    pub fn load() -> Result<Self, RegistryError> {
        match Self::default_path() {
            Some(p) => Self::load_from(&p),
            None => Ok(Self::default()),
        }
    }

    pub fn load_from(path: &Path) -> Result<Self, RegistryError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(RegistryError::Io {
                    path: path.to_path_buf(),
                    message: e.to_string(),
                });
            }
        };
        let reg: Self = toml::from_str(&text).map_err(|e| RegistryError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        if let Some(bad) = reg.pdks.keys().find(|n| !valid_name(n)) {
            return Err(RegistryError::Parse {
                path: path.to_path_buf(),
                message: format!("`{bad}` is not a usable PDK name"),
            });
        }
        Ok(reg)
    }

    /// Save to the default path, creating its folder. Returns the path.
    pub fn save(&self) -> Result<PathBuf, RegistryError> {
        let path = Self::default_path().ok_or(RegistryError::NoConfigDir)?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> Result<(), RegistryError> {
        let io = |e: String| RegistryError::Io {
            path: path.to_path_buf(),
            message: e,
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io(e.to_string()))?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| io(e.to_string()))?;
        crate::project::atomic_write(path, text.as_bytes()).map_err(|e| io(e.to_string()))
    }

    /// Check the folder and register it under `name`, replacing an entry of
    /// the same name.
    pub fn add(&mut self, name: &str, path: &Path) -> Result<Pdk, RegistryError> {
        if !valid_name(name) {
            return Err(RegistryError::BadName(name.to_string()));
        }
        let pdk = Pdk::open(name, path)?;
        self.pdks.insert(
            name.to_string(),
            RegistryEntry {
                path: pdk.root.clone(),
            },
        );
        Ok(pdk)
    }

    /// Unregister. Whether there was such an entry.
    pub fn remove(&mut self, name: &str) -> bool {
        self.pdks.remove(name).is_some()
    }

    /// Open a registered PDK, checking its folder again (it may have been
    /// moved or rebuilt since it was added).
    pub fn open(&self, name: &str) -> Result<Pdk, RegistryError> {
        let entry = self.pdks.get(name).ok_or_else(|| RegistryError::Unknown {
            name: name.to_string(),
            known: self.pdks.keys().cloned().collect(),
        })?;
        Ok(Pdk::open(name, &entry.path)?)
    }

    /// Every registered folder: the roots the netlist policy trusts.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.pdks.values().map(|e| e.path.clone()).collect()
    }
}

/// A project's `aispice.toml`. Unknown keys are ignored, so later versions
/// can add their own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProjectConfig {
    /// The registered PDK the project's circuits use.
    #[serde(default)]
    pub pdk: Option<String>,
}

impl ProjectConfig {
    /// Read `aispice.toml` in a project folder; defaults when there is none.
    pub fn load(root: &Path) -> Result<Self, String> {
        let path = root.join(PROJECT_FILE);
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(_) => return Ok(Self::default()),
        };
        if meta.len() > MAX_PROJECT_FILE {
            return Err(format!("{PROJECT_FILE} is larger than 64 KiB"));
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("{PROJECT_FILE} could not be read: {e}"))?;
        let cfg: Self =
            toml::from_str(&text).map_err(|e| format!("{PROJECT_FILE} is not valid: {e}"))?;
        if let Some(name) = &cfg.pdk
            && !valid_name(name)
        {
            return Err(format!(
                "{PROJECT_FILE}: `{name}` is not a PDK name; set pdk to the name a PDK is registered under"
            ));
        }
        Ok(cfg)
    }
}

/// `aispice.toml` text with `pdk` set to `name`, other settings kept.
pub fn with_pdk(existing: Option<&str>, name: &str) -> Result<String, String> {
    let mut table: toml::Table = match existing {
        Some(t) => toml::from_str(t).map_err(|e| format!("{PROJECT_FILE} is not valid: {e}"))?,
        None => toml::Table::new(),
    };
    table.insert("pdk".into(), toml::Value::String(name.to_string()));
    toml::to_string_pretty(&table).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_folder_names() {
        for ok in ["ihp-sg13g2", "IHP", "sky130.A", "x_1"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "",
            "-x",
            ".hidden",
            "../up",
            "a/b",
            "a b",
            "ä",
            &"x".repeat(65),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn add_save_load_open_remove() {
        let dir = tempfile::tempdir().unwrap();
        let root = aispice_sim::pdk::fake_ihp(dir.path());
        let file = dir.path().join("config/aispice/pdks.toml");
        let mut reg = Registry::load_from(&file).unwrap();
        assert!(reg.pdks.is_empty());
        // A checkout narrows to its ihp-sg13g2 folder.
        let pdk = reg
            .add("ihp-sg13g2", &dir.path().join("IHP-Open-PDK"))
            .unwrap();
        assert_eq!(pdk.root, std::fs::canonicalize(&root).unwrap());
        reg.save_to(&file).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("[pdks.ihp-sg13g2]"), "{text}");
        let back = Registry::load_from(&file).unwrap();
        assert_eq!(back, reg);
        assert_eq!(back.open("ihp-sg13g2").unwrap().root, pdk.root);
        assert_eq!(back.roots(), vec![pdk.root.clone()]);
        let err = back.open("sky130").unwrap_err().to_string();
        assert!(err.contains("registered: ihp-sg13g2"), "{err}");
        let mut back = back;
        assert!(back.remove("ihp-sg13g2"));
        assert!(!back.remove("ihp-sg13g2"));
        assert!(
            back.open("ihp-sg13g2")
                .unwrap_err()
                .to_string()
                .contains("none are registered")
        );
    }

    #[test]
    fn bad_names_and_folders_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut reg = Registry::default();
        assert!(matches!(
            reg.add("../x", dir.path()),
            Err(RegistryError::BadName(_))
        ));
        assert!(matches!(
            reg.add("x", dir.path()),
            Err(RegistryError::Pdk(_))
        ));
        assert!(reg.pdks.is_empty());
        let file = dir.path().join("pdks.toml");
        std::fs::write(&file, "[pdks.\"../evil\"]\npath = \"/tmp\"\n").unwrap();
        assert!(Registry::load_from(&file).is_err());
    }

    #[test]
    fn project_config_names_a_pdk_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            ProjectConfig::load(dir.path()).unwrap(),
            ProjectConfig::default()
        );
        std::fs::write(
            dir.path().join(PROJECT_FILE),
            "pdk = \"ihp-sg13g2\"\nfuture = 1\n",
        )
        .unwrap();
        assert_eq!(
            ProjectConfig::load(dir.path()).unwrap().pdk.as_deref(),
            Some("ihp-sg13g2")
        );
        std::fs::write(dir.path().join(PROJECT_FILE), "pdk = \"/home/me/evil\"\n").unwrap();
        assert!(ProjectConfig::load(dir.path()).is_err());
        std::fs::write(dir.path().join(PROJECT_FILE), "pdk = [\n").unwrap();
        assert!(ProjectConfig::load(dir.path()).is_err());
        std::fs::write(dir.path().join(PROJECT_FILE), "#".repeat(70 * 1024)).unwrap();
        assert!(ProjectConfig::load(dir.path()).is_err());
    }

    #[test]
    fn setting_the_pdk_keeps_other_settings() {
        let text = with_pdk(Some("future = 1\npdk = \"old\"\n"), "ihp-sg13g2").unwrap();
        let t: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(t["pdk"].as_str(), Some("ihp-sg13g2"));
        assert_eq!(t["future"].as_integer(), Some(1));
        assert_eq!(with_pdk(None, "x").unwrap().trim(), "pdk = \"x\"");
    }
}
