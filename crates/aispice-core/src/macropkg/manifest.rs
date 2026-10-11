//! `macro.toml`: what a macro package says about its block, and reading and
//! writing it.

use super::limits::{Limits, ReadError, read_capped, toml_depth};
use super::schema::{Val, validate_def};
use super::toml10::toml11_syntax;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

pub const SCHEMA_VERSION: &str = "macro/v1";
pub const SPECS_SCHEMA: &str = "macro.specs/v1";
pub const EVIDENCE_SCHEMA: &str = "macro.evidence/v1";
pub const OPENADA_SCHEMA: &str = "openada.result/v0alpha1";
pub const MANIFEST: &str = "macro.toml";

#[derive(Debug, thiserror::Error)]
pub enum MacroError {
    #[error("cannot read {path}: {message}")]
    Read { path: String, message: String },
    #[error("cannot write {path}: {message}")]
    Write { path: String, message: String },
    #[error("macro.toml does not follow macro/v1: {}", .0.join("; "))]
    Schema(Vec<String>),
    #[error("{0} is not a directory")]
    NotADirectory(String),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MacroKind {
    Analog,
    Digital,
}

impl MacroKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MacroKind::Analog => "analog",
            MacroKind::Digital => "digital",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Input,
    Output,
    Inout,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Input => "input",
            Direction::Output => "output",
            Direction::Inout => "inout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PinKind {
    Signal,
    Clock,
    Analog,
    Power,
    Ground,
}

impl PinKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PinKind::Signal => "signal",
            PinKind::Clock => "clock",
            PinKind::Analog => "analog",
            PinKind::Power => "power",
            PinKind::Ground => "ground",
        }
    }

    pub fn is_supply(self) -> bool {
        matches!(self, PinKind::Power | PinKind::Ground)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pdk {
    pub name: String,
    /// Git commit of the PDK the block was built and signed off with.
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Producer {
    pub tool: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width_um: f64,
    pub height_um: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Domain {
    pub name: String,
    pub voltage: f64,
    pub power: Vec<String>,
    pub ground: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub name: String,
    pub direction: Direction,
    pub kind: PinKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// A bus of bits `name[0]` to `name[width-1]`; a scalar without it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "integral"
    )]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The schema's `integer` also admits `2.0`, as JSON Schema does.
fn integral<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.map(|w| w as u32))
}

impl Pin {
    pub fn is_supply(&self) -> bool {
        self.kind.is_supply()
    }

    /// The pin's bit names: `q[0]`, `q[1]` for a bus, the name for a scalar.
    pub fn bits(&self) -> Vec<String> {
        match self.width {
            None => vec![self.name.clone()],
            Some(w) => (0..w).map(|i| format!("{}[{i}]", self.name)).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Corner {
    pub name: String,
    pub process: String,
    /// Degrees Celsius.
    pub temperature: f64,
    /// Supply per domain; a domain left out runs at its nominal voltage.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub voltages: BTreeMap<String, f64>,
    /// A liberty file characterized at this corner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub liberty: Option<String>,
}

/// The parsed `macro.toml`. Build one and call [`Manifest::to_toml`] to write
/// a package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Always [`SCHEMA_VERSION`].
    pub schema: String,
    pub name: String,
    pub kind: MacroKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub pdk: Pdk,
    pub producer: Producer,
    pub size: Size,
    pub domains: Vec<Domain>,
    pub pins: Vec<Pin>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub corners: Vec<Corner>,
    /// Tool-specific data, never validated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<toml::Table>,
}

impl Manifest {
    /// Parse `macro.toml` text and check it against the schema.
    pub fn from_toml_str(text: &str) -> Result<Manifest, MacroError> {
        let most = Limits::DEFAULT.nesting;
        if toml_depth(text) > most {
            return Err(MacroError::Read {
                path: MANIFEST.into(),
                message: format!("tables and arrays nest more than {most} deep"),
            });
        }
        if let Some(message) = toml11_syntax(text) {
            return Err(MacroError::Read {
                path: MANIFEST.into(),
                message,
            });
        }
        let table: toml::Table = toml::from_str(text).map_err(|e| MacroError::Read {
            path: MANIFEST.into(),
            message: e.message().to_string(),
        })?;
        let value = toml::Value::Table(table);
        let errors = validate_def("", &Val::from(&value));
        if !errors.is_empty() {
            return Err(MacroError::Schema(errors));
        }
        value
            .try_into()
            .map_err(|e: toml::de::Error| MacroError::Read {
                path: MANIFEST.into(),
                message: e.message().to_string(),
            })
    }

    /// Read `macro.toml` from a package directory (or the file itself).
    pub fn load(path: &Path) -> Result<Manifest, MacroError> {
        let file = if path.is_dir() {
            path.join(MANIFEST)
        } else {
            path.to_path_buf()
        };
        let read = |message: String| MacroError::Read {
            path: file.display().to_string(),
            message,
        };
        let bytes = read_capped(&file, Limits::DEFAULT.manifest_bytes).map_err(|e| match e {
            ReadError::TooLarge(m) => read(m),
            ReadError::Io(e) => read(e.to_string()),
        })?;
        let text = String::from_utf8(bytes).map_err(|e| read(e.to_string()))?;
        Manifest::from_toml_str(&text)
    }

    /// `macro.toml` text. Fails if the result would break the schema.
    pub fn to_toml(&self) -> Result<String, MacroError> {
        let text = toml::to_string(self).map_err(|e| MacroError::Invalid(e.to_string()))?;
        let table: toml::Table =
            toml::from_str(&text).map_err(|e| MacroError::Invalid(e.message().to_string()))?;
        let errors = validate_def("", &Val::from(&toml::Value::Table(table)));
        if errors.is_empty() {
            Ok(text)
        } else {
            Err(MacroError::Schema(errors))
        }
    }

    /// The package file name for a view: `file("lef")` is `<name>.lef`.
    pub fn file(&self, ext: &str) -> String {
        format!("{}.{ext}", self.name)
    }

    pub fn pin(&self, name: &str) -> Option<&Pin> {
        self.pins.iter().find(|p| p.name == name)
    }

    /// How many bits the pins have: a bus's width, one for a scalar.
    pub fn pin_bits(&self) -> u64 {
        self.pins.iter().map(|p| p.width.map_or(1, u64::from)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COUNTER: &str = include_str!("../../tests/fixtures/macro/tiny_counter.macro/macro.toml");
    const OTA: &str = include_str!("../../tests/fixtures/macro/tiny_ota.macro/macro.toml");

    #[test]
    fn round_trips_through_toml() {
        for text in [COUNTER, OTA] {
            let m = Manifest::from_toml_str(text).unwrap();
            let again = Manifest::from_toml_str(&m.to_toml().unwrap()).unwrap();
            assert_eq!(again, m);
        }
        let mut m = Manifest::from_toml_str(COUNTER).unwrap();
        assert_eq!(m.pin("q").unwrap().bits(), ["q[0]", "q[1]"]);
        assert_eq!(m.file("lef"), "tiny_counter.lef");
        m.name = "bad name".into();
        assert!(matches!(m.to_toml(), Err(MacroError::Schema(_))));
    }

    #[test]
    fn load_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Manifest::load(dir.path()),
            Err(MacroError::Read { .. })
        ));
        std::fs::write(dir.path().join(MANIFEST), "schema = \"macro/v1\"\n").unwrap();
        let err = Manifest::load(dir.path()).unwrap_err().to_string();
        assert!(err.contains("name is missing"), "{err}");
    }
}
