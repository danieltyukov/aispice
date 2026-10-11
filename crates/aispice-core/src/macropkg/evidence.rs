//! Evidence summaries (`macro.evidence/v1`) in a package's `evidence/` folder.
//!
//! The fields follow OpenADA's `openada.result/v0alpha1` envelope where the
//! two overlap, plus the PDK the check ran on.

use super::limits::{ReadError, too_large};
use super::manifest::{EVIDENCE_SCHEMA, MacroError, Pdk};
use super::schema::{Val, validate_def};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Fail,
    Unknown,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceTool {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Engineering {
    pub status: Status,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    /// Relative to the package for its own files, the file name otherwise.
    pub path: String,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub schema: String,
    pub operation: String,
    pub tool: EvidenceTool,
    pub pdk: Pdk,
    pub engineering: Engineering,
    pub inputs: Vec<FileRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<FileRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A file's SHA-256, read in chunks.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    match sha256_capped(path, u64::MAX) {
        Ok(digest) => Ok(digest),
        Err(ReadError::Io(e)) => Err(e),
        Err(ReadError::TooLarge(m)) => Err(std::io::Error::other(m)),
    }
}

/// A file's SHA-256, after its size says it is within `limit`.
pub fn sha256_capped(path: &Path, limit: u64) -> Result<String, ReadError> {
    let size = std::fs::metadata(path)?.len();
    if size > limit {
        return Err(ReadError::TooLarge(too_large(size, limit)));
    }
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut read = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        read = read.saturating_add(n as u64);
        if read > limit {
            // It grew after the stat.
            return Err(ReadError::TooLarge(too_large(read, limit)));
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn file_ref(path: &Path, package: &Path) -> Result<FileRef, MacroError> {
    let sha256 = sha256_file(path).map_err(|e| MacroError::Read {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    let inside = match (path.canonicalize(), package.canonicalize()) {
        (Ok(p), Ok(root)) => p
            .strip_prefix(&root)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/")),
        _ => None,
    };
    // Outside the package (a rule deck, say): the name is enough to say what it was.
    let path = inside.unwrap_or_else(|| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    Ok(FileRef { path, sha256 })
}

impl Evidence {
    /// A summary for a check that ran on files of `package`. Inputs are hashed
    /// now, so check the package after any later change to them: the
    /// validator reports stale evidence.
    pub fn new(
        package: &Path,
        operation: &str,
        status: Status,
        summary: &str,
        tool: (&str, &str),
        pdk: &Pdk,
        inputs: &[PathBuf],
    ) -> Result<Evidence, MacroError> {
        Ok(Evidence {
            schema: EVIDENCE_SCHEMA.into(),
            operation: operation.into(),
            tool: EvidenceTool {
                name: tool.0.into(),
                version: tool.1.into(),
            },
            pdk: pdk.clone(),
            engineering: Engineering {
                status,
                summary: summary.into(),
            },
            inputs: inputs
                .iter()
                .map(|p| file_ref(p, package))
                .collect::<Result<_, _>>()?,
            artifacts: Vec::new(),
            data: None,
        })
    }

    /// Hash and record files the check wrote (reports, logs).
    pub fn with_artifacts(mut self, package: &Path, files: &[PathBuf]) -> Result<Self, MacroError> {
        for f in files {
            self.artifacts.push(file_ref(f, package)?);
        }
        Ok(self)
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }

    /// Write to `evidence/<operation>.json` and return the path.
    pub fn write(&self, package: &Path) -> Result<PathBuf, MacroError> {
        let value = serde_json::to_value(self).map_err(|e| MacroError::Invalid(e.to_string()))?;
        let errors = validate_def("evidence", &Val::from(&value));
        if !errors.is_empty() {
            return Err(MacroError::Invalid(errors.join("; ")));
        }
        let folder = package.join("evidence");
        let path = folder.join(format!("{}.json", self.operation));
        let text = serde_json::to_string_pretty(&value).expect("JSON") + "\n";
        std::fs::create_dir_all(&folder)
            .and_then(|_| std::fs::write(&path, text))
            .map_err(|e| MacroError::Write {
                path: path.display().to_string(),
                message: e.to_string(),
            })?;
        Ok(path)
    }
}
