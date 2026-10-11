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
/// So is a `pdks.toml` larger than this.
const MAX_REGISTRY_FILE: u64 = 1024 * 1024;

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
    #[error("{} is {message}", .path.display())]
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
    /// `<config dir>/aispice/pdks.toml`, or the file `AISPICE_PDK_REGISTRY`
    /// names (for CI, and tests that must not touch the user's file).
    pub fn default_path() -> Option<PathBuf> {
        if let Some(p) = std::env::var_os("AISPICE_PDK_REGISTRY").filter(|p| !p.is_empty()) {
            return Some(PathBuf::from(p));
        }
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

    /// The registry in `path`; empty when there is none. It is the user's
    /// own file, so a link to it (from a dotfiles checkout, say) is
    /// followed. Errors give a position, not the text: a run's error carries
    /// them to the model.
    pub fn load_from(path: &Path) -> Result<Self, RegistryError> {
        let text = match read_small_file(path, true, MAX_REGISTRY_FILE) {
            Ok(Some(t)) => t,
            Ok(None) => return Ok(Self::default()),
            Err(e) => {
                return Err(RegistryError::Io {
                    path: path.to_path_buf(),
                    message: format!("it {e}"),
                });
            }
        };
        let reg: Self = toml::from_str(&text).map_err(|e| RegistryError::Parse {
            path: path.to_path_buf(),
            message: toml_error(&text, &e),
        })?;
        if reg.pdks.keys().any(|n| !valid_name(n)) {
            return Err(RegistryError::Parse {
                path: path.to_path_buf(),
                message: "not valid: a PDK name in it is not usable (use letters, digits, `.`, `_` and `-`, starting with a letter or digit)".into(),
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

    /// The registered names whose folder holds a PDK of this kind
    /// (`ihp-sg13g2`) and still checks out.
    pub fn names_of_kind(&self, kind: &str) -> Vec<String> {
        self.pdks
            .keys()
            .filter(|name| {
                self.open(name)
                    .is_ok_and(|p| p.kit.kind.eq_ignore_ascii_case(kind))
            })
            .cloned()
            .collect()
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

    /// The roots the netlist policy trusts: every registered folder that
    /// still checks out as a PDK, checked now and with symlinks resolved, so
    /// an entry whose folder was moved, replaced by a link or emptied since
    /// it was registered trusts nothing.
    pub fn trusted_roots(&self) -> Vec<PathBuf> {
        self.pdks
            .iter()
            .filter_map(|(name, e)| Pdk::open(name, &e.path).ok().map(|p| p.root))
            .collect()
    }
}

/// A project's `aispice.toml`. Unknown keys are ignored, so later versions
/// can add their own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectConfig {
    /// The registered PDK the project's circuits use.
    pub pdk: Option<String>,
    /// Verilog-A sources aispice compiles for the project's decks
    /// (`veriloga = ["models/comparator.va"]`), project-relative.
    pub veriloga: Vec<String>,
    /// Verilog models the project's decks co-simulate (`[[cosim]]`).
    pub cosim: Vec<CosimDecl>,
}

/// One `[[cosim]]` entry: the model name decks use on an XSPICE `A`
/// instance, the Verilog sources, and the top module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CosimDecl {
    pub model: String,
    pub sources: Vec<String>,
    pub top: String,
}

/// Behavioural models one project may declare, Verilog-A and co-simulated
/// together.
pub const MAX_BEHAVIORAL_MODELS: usize = 32;

/// A Verilog or Verilog-A name: a letter or `_`, then letters, digits and
/// `_`, at most 64 characters.
fn identifier(s: &str) -> bool {
    let mut c = s.chars();
    s.len() <= 64
        && c.next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A project-relative source path with one of the extensions: no `..`, not
/// absolute, not under aispice's own folder.
fn source_path(s: &str, exts: &[&str]) -> bool {
    let p = Path::new(s);
    let lower = s.to_ascii_lowercase();
    !s.is_empty()
        && s.len() <= 256
        && !p.is_absolute()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
        && !s.starts_with(".aispice")
        && exts.iter().any(|e| lower.ends_with(e))
}

/// A list of source paths from `aispice.toml`; the error names the key,
/// never the file's text.
fn sources(value: Option<&toml::Value>, key: &str, exts: &[&str]) -> Result<Vec<String>, String> {
    let bad = || {
        format!(
            "{PROJECT_FILE}: {key} must be a list of project-relative {} files",
            exts.join(" or ")
        )
    };
    match value {
        None => Ok(Vec::new()),
        Some(toml::Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                toml::Value::String(s) if source_path(s, exts) => Ok(s.clone()),
                _ => Err(bad()),
            })
            .collect(),
        Some(_) => Err(bad()),
    }
}

impl ProjectConfig {
    /// Read `aispice.toml` in a project folder; defaults when there is none.
    /// Errors never quote the file: they reach the model and MCP clients.
    pub fn load(root: &Path) -> Result<Self, String> {
        let Some(text) = read_project_file(root)? else {
            return Ok(Self::default());
        };
        let table: toml::Table = toml::from_str(&text)
            .map_err(|e| format!("{PROJECT_FILE} is {}", toml_error(&text, &e)))?;
        let pdk = match table.get("pdk") {
            None => None,
            Some(toml::Value::String(name)) if valid_name(name) => Some(name.clone()),
            Some(_) => {
                return Err(format!(
                    "{PROJECT_FILE}: pdk is not a PDK name; set it to the name a PDK is registered under"
                ));
            }
        };
        let veriloga = sources(table.get("veriloga"), "veriloga", &[".va", ".vams"])?;
        let mut cosim = Vec::new();
        match table.get("cosim") {
            None => {}
            Some(toml::Value::Array(entries)) => {
                for (i, entry) in entries.iter().enumerate() {
                    let at = format!("{PROJECT_FILE}: [[cosim]] entry {}", i + 1);
                    let toml::Value::Table(t) = entry else {
                        return Err(format!("{at} is not a table"));
                    };
                    if t.keys()
                        .any(|k| !matches!(k.as_str(), "model" | "sources" | "top"))
                    {
                        return Err(format!("{at} takes only model, sources and top"));
                    }
                    let name = |key: &str| match t.get(key) {
                        Some(toml::Value::String(s)) if identifier(s) => Ok(Some(s.clone())),
                        None => Ok(None),
                        Some(_) => Err(format!(
                            "{at}: {key} must be a Verilog name (letters, digits and _, starting with a letter)"
                        )),
                    };
                    let model = name("model")?.ok_or_else(|| format!("{at} needs a model name"))?;
                    let top = name("top")?.unwrap_or_else(|| model.clone());
                    let srcs =
                        sources(t.get("sources"), "sources", &[".v", ".sv"]).map_err(|e| {
                            format!(
                                "{at}: {}",
                                e.trim_start_matches(&format!("{PROJECT_FILE}: "))
                            )
                        })?;
                    if srcs.is_empty() {
                        return Err(format!("{at} needs at least one source"));
                    }
                    if cosim
                        .iter()
                        .any(|c: &CosimDecl| c.model.eq_ignore_ascii_case(&model))
                    {
                        return Err(format!("{at} repeats the model name {model}"));
                    }
                    cosim.push(CosimDecl {
                        model,
                        sources: srcs,
                        top,
                    });
                }
            }
            Some(_) => return Err(format!("{PROJECT_FILE}: cosim must be [[cosim]] tables")),
        }
        if veriloga.len() + cosim.len() > MAX_BEHAVIORAL_MODELS {
            return Err(format!(
                "{PROJECT_FILE} declares more than {MAX_BEHAVIORAL_MODELS} behavioural models"
            ));
        }
        Ok(Self {
            pdk,
            veriloga,
            cosim,
        })
    }
}

/// The text of `aispice.toml` in a project folder, `None` when there is
/// none. The file is part of the project and so untrusted: a link is refused
/// (it could point at a key or a token anywhere on disk), and so is anything
/// but a regular file.
pub fn read_project_file(root: &Path) -> Result<Option<String>, String> {
    read_small_file(&root.join(PROJECT_FILE), false, MAX_PROJECT_FILE)
        .map_err(|e| format!("{PROJECT_FILE} {e}"))
}

/// A small text file, `None` when there is none. Only a regular file is
/// read (a FIFO would block, a device would never end), a link only when
/// `follow_links`, and at most `limit` bytes. Errors say what is wrong with
/// the file, not what is in it, and do not name it.
fn read_small_file(path: &Path, follow_links: bool, limit: u64) -> Result<Option<String>, String> {
    use std::io::Read;
    let meta = if follow_links {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    };
    let meta = match meta {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("could not be read: {e}")),
    };
    if meta.file_type().is_symlink() {
        return Err("is a link; make it a plain file".into());
    }
    if !meta.is_file() {
        return Err("is not a regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(limit + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("could not be read: {e}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("is larger than {} KiB", limit / 1024));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "is not UTF-8 text".into())
}

/// "not valid TOML at line L, column C". toml's own message quotes the line
/// it failed on, and the file may hold anything.
fn toml_error(text: &str, e: &toml::de::Error) -> String {
    let Some(span) = e.span() else {
        return "not valid TOML".into();
    };
    let before = &text.as_bytes()[..span.start.min(text.len())];
    let line_start = before
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let line = before.iter().filter(|&&b| b == b'\n').count() + 1;
    let column = String::from_utf8_lossy(&before[line_start..])
        .chars()
        .count()
        + 1;
    format!("not valid TOML at line {line}, column {column}")
}

/// `aispice.toml` text with `pdk` set to `name`, other settings kept.
pub fn with_pdk(existing: Option<&str>, name: &str) -> Result<String, String> {
    let mut table: toml::Table = match existing {
        Some(t) => {
            toml::from_str(t).map_err(|e| format!("{PROJECT_FILE} is {}", toml_error(t, &e)))?
        }
        None => toml::Table::new(),
    };
    table.insert("pdk".into(), toml::Value::String(name.to_string()));
    toml::to_string_pretty(&table).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn behavioural_models_are_declared_by_path_and_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(PROJECT_FILE),
            "veriloga = [\"models/comp.va\"]\n[[cosim]]\nmodel = \"counter\"\nsources = [\"rtl/counter.v\", \"rtl/inc.sv\"]\n[[cosim]]\nmodel = \"div\"\ntop = \"divider\"\nsources = [\"rtl/div.v\"]\n",
        )
        .unwrap();
        let c = ProjectConfig::load(dir.path()).unwrap();
        assert_eq!(c.veriloga, ["models/comp.va"]);
        assert_eq!(c.cosim.len(), 2);
        assert_eq!(c.cosim[0].top, "counter");
        assert_eq!(c.cosim[1].top, "divider");
        for (text, why) in [
            ("veriloga = [\"../x.va\"]\n", "veriloga must be"),
            ("veriloga = [\"/etc/x.va\"]\n", "veriloga must be"),
            ("veriloga = [\".aispice/build/x.va\"]\n", "veriloga must be"),
            ("veriloga = [\"lib.so\"]\n", "veriloga must be"),
            (
                "[[cosim]]\nmodel = \"c\"\nsources = [\"lib.so\"]\n",
                "sources must be",
            ),
            (
                "[[cosim]]\nmodel = \"1c\"\nsources = [\"c.v\"]\n",
                "model must be",
            ),
            (
                "[[cosim]]\nmodel = \"c\"\nsources = []\n",
                "at least one source",
            ),
            (
                "[[cosim]]\nmodel = \"c\"\nsources = [\"c.v\"]\nsimulation = \"x.so\"\n",
                "takes only",
            ),
            (
                "[[cosim]]\nmodel = \"c\"\nsources = [\"c.v\"]\n[[cosim]]\nmodel = \"C\"\nsources = [\"d.v\"]\n",
                "repeats",
            ),
        ] {
            std::fs::write(dir.path().join(PROJECT_FILE), text).unwrap();
            let e = ProjectConfig::load(dir.path()).unwrap_err();
            assert!(e.contains(why), "{text}: {e}");
        }
    }

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
        assert_eq!(back.trusted_roots(), vec![pdk.root.clone()]);
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
    fn broad_roots_are_refused_at_registration() {
        let mut reg = Registry::default();
        let err = reg.add("root", Path::new("/")).unwrap_err();
        assert!(err.to_string().contains("too broad"), "{err}");
        if let Some(home) = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_dir())
        {
            let err = reg.add("home", &home).unwrap_err();
            assert!(err.to_string().contains("too broad"), "{err}");
        }
        assert!(reg.pdks.is_empty());
    }

    /// A registered folder replaced afterwards (here by a link to a folder
    /// without the PDK layout) is no longer trusted.
    #[cfg(unix)]
    #[test]
    fn roots_are_checked_again_when_used() {
        let dir = tempfile::tempdir().unwrap();
        let root = aispice_sim::pdk::fake_ihp(dir.path());
        let mut reg = Registry::default();
        reg.add("ihp", &root).unwrap();
        assert_eq!(reg.trusted_roots().len(), 1);
        let moved = dir.path().join("moved");
        std::fs::rename(&root, &moved).unwrap();
        std::os::unix::fs::symlink(dir.path(), &root).unwrap();
        assert!(reg.trusted_roots().is_empty());
        assert!(reg.open("ihp").is_err());
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

    /// Was a hole: a project could ship `aispice.toml` as a link to a file
    /// elsewhere (a token in a dotfile, a key), and TOML's error quoted the
    /// line it failed on in every tool result that builds a netlist.
    #[cfg(unix)]
    #[test]
    fn a_linked_project_file_is_refused_unread() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret.env");
        std::fs::write(&secret, "API_KEY=sk-live-0123456789\n").unwrap();
        let root = dir.path().join("project");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&secret, root.join(PROJECT_FILE)).unwrap();
        let err = ProjectConfig::load(&root).unwrap_err();
        assert!(!err.contains("sk-live") && err.contains("link"), "{err}");
        // A link to a valid file inside the project is refused as well.
        std::fs::remove_file(root.join(PROJECT_FILE)).unwrap();
        std::fs::write(root.join("other.toml"), "pdk = \"ihp\"\n").unwrap();
        std::os::unix::fs::symlink(root.join("other.toml"), root.join(PROJECT_FILE)).unwrap();
        assert!(ProjectConfig::load(&root).is_err());
        assert!(read_project_file(&root).is_err());
    }

    /// A FIFO would block the read forever and a device would never end.
    #[cfg(unix)]
    #[test]
    fn a_project_file_that_is_not_a_regular_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join(PROJECT_FILE);
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success());
        if !made {
            eprintln!("skipped: no mkfifo");
            return;
        }
        let root = dir.path().to_path_buf();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(ProjectConfig::load(&root));
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("reading a FIFO as aispice.toml blocked");
        assert!(result.unwrap_err().contains("not a regular file"));
    }

    #[test]
    fn toml_errors_give_a_position_not_the_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(PROJECT_FILE),
            "pdk = \"ihp\"\nAPI_KEY=sk-live-0123456789\n",
        )
        .unwrap();
        let err = ProjectConfig::load(dir.path()).unwrap_err();
        assert!(
            !err.contains("sk-live") && err.contains("line 2, column 9"),
            "{err}"
        );
        for text in ["pdk = \"sk-live-x y\"\n", "pdk = 123456789\n"] {
            std::fs::write(dir.path().join(PROJECT_FILE), text).unwrap();
            let err = ProjectConfig::load(dir.path()).unwrap_err();
            assert!(!err.contains("sk-live") && !err.contains("12345"), "{err}");
        }
        let err = with_pdk(Some("x = sk-live-0123456789\n"), "ihp").unwrap_err();
        assert!(!err.contains("sk-live") && err.contains("line 1"), "{err}");

        let file = dir.path().join("pdks.toml");
        std::fs::write(&file, "[pdks.ihp]\npath = /home/me/sk-live-0123456789\n").unwrap();
        let err = Registry::load_from(&file).unwrap_err().to_string();
        assert!(
            !err.contains("sk-live") && err.contains("line 2, column 8"),
            "{err}"
        );
        std::fs::write(&file, "[pdks.\"sk-live x\"]\npath = \"/tmp\"\n").unwrap();
        let err = Registry::load_from(&file).unwrap_err().to_string();
        assert!(!err.contains("sk-live"), "{err}");
        std::fs::write(&file, "[pdks.ihp]\npath = 123456789\n").unwrap();
        let err = Registry::load_from(&file).unwrap_err().to_string();
        assert!(!err.contains("12345"), "{err}");
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
