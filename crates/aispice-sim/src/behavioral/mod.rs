//! Behavioural models aispice compiles and loads itself.
//!
//! Two kinds of native code reach a simulation here: Verilog-A modules,
//! compiled by OpenVAF into OSDI libraries that ngspice loads, and Verilog
//! modules, compiled by Verilator into libraries that ngspice's XSPICE
//! `d_cosim` code model runs. A deck can never name either: the netlist
//! policy refuses `osdi` lines and `d_cosim` and `d_process` model cards.
//! aispice checks the sources ([`veriloga::check`], [`verilog::check`]),
//! builds them in a private folder from argument vectors
//! ([`veriloga::build`], [`verilog::build`]), and hands the bytes to the
//! backend, which writes them into the run's own private folder, checks
//! their SHA-256 and loads them.

pub mod scan;
pub mod verilog;
pub mod veriloga;

use crate::backend::SimError;
use crate::backend::process::{self, ProcessSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A Verilog-A source file is at most this large.
pub const MAX_VERILOGA_BYTES: usize = 1 << 20;
/// The Verilog sources of one co-simulated model: this many files at most...
pub const MAX_VERILOG_FILES: usize = 16;
/// ...each at most this large...
pub const MAX_VERILOG_FILE_BYTES: usize = 1 << 20;
/// ...and together at most this large.
pub const MAX_VERILOG_BYTES: usize = 4 << 20;
/// A built library larger than this is refused rather than kept in memory.
pub const MAX_LIBRARY_BYTES: u64 = 64 << 20;
/// How long OpenVAF may take on one file.
pub const OPENVAF_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a Verilator build may take, all its steps together.
pub const VERILATOR_TIMEOUT: Duration = Duration::from_secs(600);

/// A source file read from the project, checked for size by the caller.
#[derive(Debug, Clone)]
pub struct Source {
    /// The project-relative path, for messages.
    pub name: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryKind {
    /// An OSDI library from Verilog-A, loaded with `osdi`.
    Osdi,
    /// A Verilator build for XSPICE `d_cosim`.
    Cosim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Port {
    pub name: String,
    pub direction: Direction,
    /// Bits; 1 for a scalar, and for every Verilog-A port.
    pub width: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Module {
    pub name: String,
    pub ports: Vec<Port>,
}

/// A library aispice built, with the SHA-256 of its bytes. Only the runner
/// that built it holds one, so a library on disk is never trusted.
#[derive(Debug, Clone)]
pub struct Library {
    pub kind: LibraryKind,
    /// The file stem in a run folder; for co-simulation also the model name
    /// decks use. Letters, digits and `_` only.
    pub name: String,
    pub modules: Vec<Module>,
    pub bytes: Arc<[u8]>,
    pub sha256: String,
    /// The compiler that built it, with its version.
    pub tool: String,
}

impl Library {
    /// The library's file name in a run folder, lower case: ngspice
    /// lower-cases the `d_cosim` card that names it.
    pub fn file_name(&self) -> String {
        let name = self.name.to_ascii_lowercase();
        match self.kind {
            LibraryKind::Osdi => format!("{name}.osdi"),
            LibraryKind::Cosim => format!("cosim_{name}.so"),
        }
    }

    /// `d_cosim`'s pins for this library's top module: inputs, outputs and
    /// inouts, each bus most significant bit first, as an instance lists
    /// them (`A1 [in...] [out...] null model`).
    pub fn pin_groups(&self) -> [Vec<String>; 3] {
        let mut groups: [Vec<String>; 3] = Default::default();
        if let Some(m) = self.modules.first() {
            for p in &m.ports {
                let g = match p.direction {
                    Direction::Input => 0,
                    Direction::Output => 1,
                    Direction::Inout => 2,
                };
                if p.width == 1 {
                    groups[g].push(p.name.clone());
                } else {
                    for b in (0..p.width).rev() {
                        groups[g].push(format!("{}[{b}]", p.name));
                    }
                }
            }
        }
        groups
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    /// The source breaks a rule; nothing was compiled.
    #[error("{file}:{line}: {message}")]
    Source {
        file: String,
        line: usize,
        message: String,
    },
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Missing(String),
    /// The compiler ran and failed; its messages, private paths replaced.
    #[error("{tool} failed:\n{log}")]
    Failed { tool: String, log: String },
    #[error("{0}")]
    Io(String),
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Letters, digits and `_`; at most 64 characters, never empty, never
/// starting with a digit.
pub fn safe_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(64)
        .collect();
    match s.chars().next() {
        None => "model".into(),
        Some(c) if c.is_ascii_digit() => format!("m{}", &s[..s.len().min(63)]),
        Some(_) => s,
    }
}

/// A private folder for one build, readable only by this user.
pub(crate) fn build_dir() -> Result<tempfile::TempDir, BuildError> {
    tempfile::Builder::new()
        .prefix("aispice-build-")
        .tempdir()
        .map_err(|e| BuildError::Io(format!("cannot create a build folder: {e}")))
}

/// Write a file into the build folder.
pub(crate) fn write(dir: &Path, name: &str, bytes: &[u8]) -> Result<PathBuf, BuildError> {
    let p = dir.join(name);
    std::fs::write(&p, bytes)
        .map_err(|e| BuildError::Io(format!("cannot write {name} for the build: {e}")))?;
    Ok(p)
}

/// Read a built library, refusing one over [`MAX_LIBRARY_BYTES`].
pub(crate) fn read_library(path: &Path) -> Result<Vec<u8>, BuildError> {
    let size = std::fs::metadata(path)
        .map_err(|e| BuildError::Io(format!("the build wrote no library: {e}")))?
        .len();
    if size > MAX_LIBRARY_BYTES {
        return Err(BuildError::Refused(format!(
            "the built library is {size} bytes, over the {MAX_LIBRARY_BYTES} byte limit"
        )));
    }
    std::fs::read(path).map_err(|e| BuildError::Io(format!("cannot read the library: {e}")))
}

/// Run one build step in `dir` with `HOME` there, so no start-up file of the
/// user's is read, within what is left of `deadline`.
pub(crate) async fn step(
    program: &Path,
    args: &[String],
    dir: &Path,
    deadline: std::time::Instant,
    cancel: &CancellationToken,
) -> Result<process::ProcessOutput, BuildError> {
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return Err(BuildError::Refused("the build took too long".into()));
    }
    let mut spec = ProcessSpec::new(program, dir)
        .env("HOME", dir)
        .env("USERPROFILE", dir);
    for a in args {
        spec = spec.arg(a);
    }
    process::run(&spec, left, cancel)
        .await
        .map_err(|e| match e {
            SimError::Timeout(_) => {
                BuildError::Refused("the build took too long and was stopped".into())
            }
            SimError::Cancelled => BuildError::Refused("the build was cancelled".into()),
            other => BuildError::Io(other.to_string()),
        })
}

/// A compiler's messages for people: terminal colours removed and the
/// private build folder's path replaced by `name`, the source's project
/// path, so a message neither leaks nor confuses with a temporary path.
pub(crate) fn clean_log(log: &str, dir: &Path, sources: &[(String, String)]) -> String {
    let mut out = strip_ansi(log);
    for (file, name) in sources {
        let private = dir.join(file).display().to_string();
        out = out.replace(&private, name);
    }
    out = out.replace(&dir.display().to_string(), "<build>");
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    // The start of a long log is where compilers report the first error.
    let keep = 60;
    if lines.len() > keep {
        format!(
            "{}\n[{} more lines]",
            lines[..keep].join("\n"),
            lines.len() - keep
        )
    } else {
        lines.join("\n")
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// A program's version line, from `--version`, within ten seconds.
pub async fn version(program: &Path) -> Option<String> {
    let dir = build_dir().ok()?;
    let out = step(
        program,
        &["--version".to_string()],
        dir.path(),
        std::time::Instant::now() + Duration::from_secs(10),
        &CancellationToken::new(),
    )
    .await
    .ok()?;
    out.combined()
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| strip_ansi(l).chars().take(120).collect())
}

/// What a machine has for behavioural models, for `aispice doctor`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Toolchain {
    pub openvaf: Option<(PathBuf, String)>,
    pub verilator: Option<(PathBuf, String)>,
    pub cxx: Option<PathBuf>,
    pub make: Option<PathBuf>,
    /// ngspice's `d_cosim` shim sources, found next to the ngspice in use.
    pub cosim_shim: Option<PathBuf>,
}

impl Toolchain {
    pub async fn detect(
        openvaf: Option<&Path>,
        verilator_exe: Option<&Path>,
        ngspice: Option<&Path>,
    ) -> Toolchain {
        let find = |configured: Option<&Path>, name: &str| match configured {
            Some(p) => p.is_file().then(|| p.to_path_buf()),
            None => crate::backend::which(name),
        };
        let openvaf = match find(openvaf, "openvaf") {
            Some(p) => version(&p).await.map(|v| (p, v)),
            None => None,
        };
        let verilator = match find(verilator_exe, "verilator") {
            Some(p) => version(&p).await.map(|v| (p, v)),
            None => None,
        };
        let ngspice = match ngspice {
            Some(p) => Some(p.to_path_buf()),
            None => crate::backend::which("ngspice"),
        };
        Toolchain {
            openvaf,
            verilator,
            cxx: verilog::cxx(),
            make: crate::backend::which("make"),
            cosim_shim: ngspice.as_deref().and_then(verilog::shim_dir),
        }
    }

    /// Why co-simulation cannot be built here, if it cannot.
    pub fn cosim_missing(&self) -> Option<String> {
        let mut missing = Vec::new();
        if self.verilator.is_none() {
            missing.push("Verilator");
        }
        if self.cxx.is_none() {
            missing.push("a C++ compiler (g++ or clang++)");
        }
        if self.make.is_none() {
            missing.push("make");
        }
        if self.cosim_shim.is_none() {
            missing.push("ngspice's d_cosim sources (ngspice 42 or newer installs them under share/ngspice/scripts/src)");
        }
        (!missing.is_empty()).then(|| missing.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_logs() {
        assert_eq!(safe_name("counter"), "counter");
        assert_eq!(safe_name("my-model.v"), "my_model_v");
        assert_eq!(safe_name("4bit"), "m4bit");
        assert_eq!(safe_name(""), "model");
        let dir = Path::new("/tmp/aispice-build-x");
        let log = "\u{1b}[1m\u{1b}[31merror\u{1b}[0m: bad\n  --> /tmp/aispice-build-x/comparator.va:3:4\n\n /tmp/aispice-build-x/obj/x.h";
        assert_eq!(
            clean_log(
                log,
                dir,
                &[("comparator.va".into(), "models/comparator.va".into())]
            ),
            "error: bad\n  --> models/comparator.va:3:4\n <build>/obj/x.h"
        );
    }

    #[test]
    fn cosim_pins_follow_ports_msb_first() {
        let lib = Library {
            kind: LibraryKind::Cosim,
            name: "counter".into(),
            modules: vec![Module {
                name: "counter".into(),
                ports: vec![
                    Port {
                        name: "clk".into(),
                        direction: Direction::Input,
                        width: 1,
                    },
                    Port {
                        name: "q".into(),
                        direction: Direction::Output,
                        width: 3,
                    },
                    Port {
                        name: "en".into(),
                        direction: Direction::Input,
                        width: 1,
                    },
                ],
            }],
            bytes: Arc::from(Vec::new()),
            sha256: String::new(),
            tool: String::new(),
        };
        assert_eq!(
            lib.pin_groups(),
            [
                vec!["clk".to_string(), "en".to_string()],
                vec!["q[2]".to_string(), "q[1]".to_string(), "q[0]".to_string()],
                vec![],
            ]
        );
        assert_eq!(lib.file_name(), "cosim_counter.so");
    }
}
