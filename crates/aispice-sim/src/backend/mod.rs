//! Simulator backends behind one trait.
//!
//! A backend takes a finished deck and returns datasets, measurements and
//! diagnostics. It does not translate or resolve models (see
//! [`crate::dialect`] and [`crate::models`]), and it does not decide whether
//! a deck is safe to run: that is the caller's netlist policy.
//!
//! Every run happens in a fresh private temporary directory that holds only
//! the deck and the files aispice writes for it, never in the user's project
//! folder, because simulators read start-up files from their working
//! directory (ngspice runs `.spiceinit`, which can execute shell commands).
//! Only the outputs are copied to the job's `run_dir` afterwards.

pub mod ltspice;
pub mod ngspice;
pub mod process;
pub mod spectre;
pub mod xyce;

use crate::dataset::Dataset;
use crate::log::Measurement;
use crate::raw::RawError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub use ltspice::Ltspice;
pub use ngspice::Ngspice;
pub use spectre::{Spectre, SpectreConfig};
pub use xyce::Xyce;

/// Raw files larger than this are refused rather than read into memory.
pub const RAW_SIZE_LIMIT: u64 = 256 << 20;
/// Log files are read up to this size.
pub const LOG_SIZE_LIMIT: u64 = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SimId {
    Ngspice,
    Xyce,
    Ltspice,
    Spectre,
}

impl SimId {
    pub const ALL: [SimId; 4] = [SimId::Ltspice, SimId::Ngspice, SimId::Xyce, SimId::Spectre];

    pub fn name(self) -> &'static str {
        match self {
            SimId::Ngspice => "ngspice",
            SimId::Xyce => "Xyce",
            SimId::Ltspice => "LTspice",
            SimId::Spectre => "Spectre",
        }
    }

    /// The dialect a deck must be translated to for this simulator.
    pub fn target(self) -> crate::dialect::Target {
        use crate::dialect::Target;
        match self {
            SimId::Ngspice => Target::Ngspice,
            SimId::Xyce => Target::Xyce,
            SimId::Ltspice => Target::Ltspice,
            SimId::Spectre => Target::Spectre,
        }
    }
}

impl std::fmt::Display for SimId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    pub found: bool,
    pub version: Option<String>,
    pub path: Option<PathBuf>,
    /// How it was found, what is missing, what mode it will run in.
    pub notes: Vec<String>,
}

/// One simulation to run.
#[derive(Debug, Clone)]
pub struct SimJob {
    /// The final deck, already translated for the simulator and its models
    /// resolved. Callers must check it with the netlist policy
    /// (`aispice_core::netlist::check_policy`) before running it: a deck can
    /// include files, and in ngspice can carry `.control` scripts, so the
    /// policy is what keeps a schematic from reaching outside the project.
    pub netlist_text: String,
    /// Where the deck and its outputs are kept after the run. The simulator
    /// never runs here; it runs in a private temporary directory and only
    /// the files it produced are moved here.
    pub run_dir: PathBuf,
    /// Base name of the deck and its outputs. Reduced to letters, digits,
    /// `_` and `-`, so it is safe as a file name and a command argument.
    pub deck_name: String,
    pub timeout: Duration,
    /// Compiled device models from a registered PDK, which the backend loads
    /// outside the deck (ngspice through its generated `.spiceinit`, Xyce
    /// with `-plugin`). Set only by aispice, after the paths passed
    /// `aispice_core::netlist::policy::check_module`; deck text can never
    /// add to it. `Some` also runs ngspice in its native mode, which PDK
    /// model libraries need.
    pub pdk: Option<crate::pdk::PdkRun>,
    /// Behavioural model libraries aispice built in this process (Verilog-A
    /// as OSDI, Verilog for `d_cosim`). ngspice writes their bytes into its
    /// private run folder, checks each SHA-256 and loads them; the other
    /// backends refuse a job that has any.
    pub libraries: Vec<std::sync::Arc<crate::behavioral::Library>>,
}

impl SimJob {
    pub fn new(netlist_text: impl Into<String>, run_dir: impl Into<PathBuf>) -> Self {
        Self {
            netlist_text: netlist_text.into(),
            run_dir: run_dir.into(),
            deck_name: "deck".into(),
            timeout: Duration::from_secs(120),
            pdk: None,
            libraries: Vec::new(),
        }
    }

    /// Refuse a job with behavioural libraries on a backend that cannot
    /// load them.
    pub fn ngspice_only(&self, sim: SimId) -> Result<(), SimError> {
        if self.libraries.is_empty() {
            return Ok(());
        }
        Err(SimError::NgspiceOnly(sim.name().to_string()))
    }

    /// The sanitised deck name.
    pub fn stem(&self) -> String {
        sanitize_name(&self.deck_name)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SimOutput {
    pub datasets: Vec<Dataset>,
    /// The simulator's log or console output, Wine noise removed.
    pub log: String,
    pub measurements: Vec<Measurement>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub duration: Duration,
    /// The main raw file in `run_dir`, when one was written.
    pub raw_path: Option<PathBuf>,
    /// Every file kept in `run_dir`, the deck included.
    pub files: Vec<PathBuf>,
}

impl SimOutput {
    /// The first dataset of an analysis kind.
    pub fn dataset(&self, kind: crate::dataset::AnalysisKind) -> Option<&Dataset> {
        self.datasets.iter().find(|d| d.kind == kind)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("{0} was not found; install it or set its path in the configuration")]
    NotFound(String),
    #[error("configuration: {0}")]
    Config(String),
    #[error("could not start {program}: {message}")]
    Spawn { program: String, message: String },
    #[error("the simulation did not finish within {0:?} and was stopped")]
    Timeout(Duration),
    #[error("the simulation was cancelled")]
    Cancelled,
    #[error("the simulation failed: {}", errors.join("; "))]
    Failed { errors: Vec<String>, log: String },
    #[error("{0}")]
    NoOutput(String),
    #[error("{what} is {size} bytes, over the {limit} byte limit")]
    TooLarge { what: String, size: u64, limit: u64 },
    #[error("{0} is not supported here")]
    Unsupported(String),
    #[error("Verilog-A models and Verilog co-simulation run on ngspice only, not on {0}")]
    NgspiceOnly(String),
    #[error(transparent)]
    Raw(#[from] RawError),
    #[error("{0}")]
    Io(String),
}

/// A simulator aispice can run.
///
/// Implementations spawn processes only from argument vectors, pass no
/// deck-controlled text on the command line other than the deck file they
/// wrote, and stop the whole process tree on timeout or cancel.
#[async_trait::async_trait]
pub trait Simulator: Send + Sync {
    fn id(&self) -> SimId;
    async fn detect(&self) -> Detection;
    async fn run(&self, job: &SimJob, cancel: &CancellationToken) -> Result<SimOutput, SimError>;
}

/// Backends with their default configuration.
pub fn simulator(id: SimId) -> Box<dyn Simulator> {
    match id {
        SimId::Ngspice => Box::new(Ngspice::default()),
        SimId::Xyce => Box::new(Xyce::default()),
        SimId::Ltspice => Box::new(Ltspice::default()),
        SimId::Spectre => Box::new(Spectre::default()),
    }
}

/// Detect every simulator with its default configuration, concurrently.
pub async fn detect_all() -> Vec<(SimId, Detection)> {
    let sims: Vec<Box<dyn Simulator>> = SimId::ALL.iter().map(|&id| simulator(id)).collect();
    let found = futures::future::join_all(sims.iter().map(|s| s.detect())).await;
    SimId::ALL.into_iter().zip(found).collect()
}

/// Choose a simulator: the first in `preference` that is available, else the
/// first available in aispice's default order (LTspice for exact LTspice
/// semantics, then ngspice, then Xyce). Spectre is only used when asked for,
/// since it runs on a remote server.
pub fn pick(preference: &[SimId], available: &[(SimId, Detection)]) -> Option<SimId> {
    let is_found = |id: SimId| available.iter().any(|(s, d)| *s == id && d.found);
    preference
        .iter()
        .copied()
        .find(|&id| is_found(id))
        .or_else(|| {
            [SimId::Ltspice, SimId::Ngspice, SimId::Xyce]
                .into_iter()
                .find(|&id| is_found(id))
        })
}

/// Letters, digits, `_` and `-` only, at most 64 characters, never empty.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let s = s.trim_start_matches('-').to_string();
    if s.is_empty() { "deck".into() } else { s }
}

/// Look a program up on `PATH`, as a shell would, without running a shell.
pub fn which(program: &str) -> Option<PathBuf> {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return p.is_file().then(|| p.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.BAT;.CMD".into())
            .split(';')
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

/// A private temporary directory for one run, holding the deck.
pub(crate) struct Workspace {
    pub dir: tempfile::TempDir,
}

impl Workspace {
    /// A new directory readable only by the current user (tempfile creates
    /// it with mode 0700 on Unix).
    pub fn new() -> Result<Self, SimError> {
        let dir = tempfile::Builder::new()
            .prefix("aispice-run-")
            .tempdir()
            .map_err(|e| SimError::Io(format!("cannot create a run directory: {e}")))?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, SimError> {
        let p = self.file(name);
        std::fs::write(&p, bytes)
            .map_err(|e| SimError::Io(format!("cannot write {}: {e}", p.display())))?;
        Ok(p)
    }

    /// Move the named files that exist into `run_dir`, returning their new
    /// paths. Missing files are skipped. Nothing here follows a symlink in
    /// `run_dir`: the folder itself must be a plain directory, an existing
    /// entry at a target name is removed first (a link is removed, not
    /// followed), and copies create their files exclusively.
    pub fn keep(&self, run_dir: &Path, names: &[String]) -> Result<Vec<PathBuf>, SimError> {
        let io = |what: &str, e: std::io::Error| SimError::Io(format!("{what}: {e}"));
        match std::fs::symlink_metadata(run_dir) {
            Ok(m) if m.file_type().is_symlink() || !m.is_dir() => {
                return Err(SimError::Io(format!(
                    "{} is not a plain folder",
                    run_dir.display()
                )));
            }
            Ok(_) => {}
            Err(_) => std::fs::create_dir_all(run_dir)
                .map_err(|e| io(&format!("cannot create {}", run_dir.display()), e))?,
        }
        let mut out = Vec::new();
        for name in names {
            let from = self.file(name);
            if !from.exists() {
                continue;
            }
            let to = run_dir.join(name);
            if let Ok(m) = std::fs::symlink_metadata(&to) {
                let removed = if m.is_dir() && !m.file_type().is_symlink() {
                    std::fs::remove_dir_all(&to)
                } else {
                    std::fs::remove_file(&to)
                };
                removed.map_err(|e| io(&format!("cannot replace {}", to.display()), e))?;
            }
            let moved =
                std::fs::rename(&from, &to).is_ok() || copy_tree_exclusive(&from, &to).is_ok();
            if !moved {
                return Err(SimError::Io(format!(
                    "cannot keep {} in {}",
                    name,
                    run_dir.display()
                )));
            }
            out.push(to);
        }
        Ok(out)
    }
}

/// Copy a file or folder, creating every destination entry fresh so an
/// existing link at the destination makes the copy fail instead of writing
/// through it.
fn copy_tree_exclusive(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other("refusing to copy a symlink"));
    }
    if meta.is_dir() {
        std::fs::create_dir(to)?;
        for e in std::fs::read_dir(from)?.flatten() {
            copy_tree_exclusive(&e.path(), &to.join(e.file_name()))?;
        }
        Ok(())
    } else {
        let mut src = std::fs::File::open(from)?;
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(to)?;
        std::io::copy(&mut src, &mut dst).map(|_| ())
    }
}

/// Read a raw file (or Spectre raw directory) within the size limit.
pub(crate) fn read_raw_limited(path: &Path) -> Result<Vec<Dataset>, SimError> {
    let size = dir_size(path);
    if size > RAW_SIZE_LIMIT {
        return Err(SimError::TooLarge {
            what: format!("raw output {}", path.display()),
            size,
            limit: RAW_SIZE_LIMIT,
        });
    }
    Ok(crate::raw::read_raw_file(path)?)
}

fn dir_size(path: &Path) -> u64 {
    if path.is_dir() {
        std::fs::read_dir(path)
            .map(|rd| rd.flatten().map(|e| dir_size(&e.path())).sum())
            .unwrap_or(0)
    } else {
        path.metadata().map(|m| m.len()).unwrap_or(0)
    }
}

/// Read a log file up to the log limit; a longer log is cut with a note.
pub(crate) fn read_log_limited(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(LOG_SIZE_LIMIT + 1).read_to_end(&mut buf).ok()?;
    if buf.len() as u64 > LOG_SIZE_LIMIT {
        buf.truncate(LOG_SIZE_LIMIT as usize);
        buf.extend_from_slice(b"\n[aispice: log truncated]\n");
    }
    Some(buf)
}

/// Turn a finished run into an output or an error: a run that produced no
/// data and reported errors (or exited non-zero) failed.
pub(crate) fn finish(
    mut out: SimOutput,
    exit_ok: bool,
    no_data_message: &str,
) -> Result<SimOutput, SimError> {
    if out.datasets.is_empty() && out.measurements.is_empty() {
        if !out.errors.is_empty() || !exit_ok {
            if out.errors.is_empty() {
                out.errors.push(no_data_message.to_string());
            }
            return Err(SimError::Failed {
                errors: out.errors,
                log: out.log,
            });
        }
        out.warnings.push(no_data_message.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitised() {
        assert_eq!(sanitize_name("rc low-pass.asc"), "rc_low-pass_asc");
        assert_eq!(sanitize_name("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize_name("--rm"), "rm");
        assert_eq!(sanitize_name(""), "deck");
        assert_eq!(sanitize_name(&"x".repeat(100)).len(), 64);
    }

    #[test]
    fn pick_prefers_then_falls_back() {
        let det = |found| Detection {
            found,
            ..Default::default()
        };
        let avail = vec![
            (SimId::Ltspice, det(false)),
            (SimId::Ngspice, det(true)),
            (SimId::Xyce, det(true)),
            (SimId::Spectre, det(true)),
        ];
        assert_eq!(pick(&[SimId::Xyce], &avail), Some(SimId::Xyce));
        assert_eq!(pick(&[SimId::Ltspice], &avail), Some(SimId::Ngspice));
        assert_eq!(pick(&[], &avail), Some(SimId::Ngspice));
        assert_eq!(pick(&[SimId::Spectre], &avail), Some(SimId::Spectre));
        let none = vec![(SimId::Spectre, det(true))];
        assert_eq!(pick(&[], &none), None);
    }

    #[test]
    fn which_finds_sh() {
        if cfg!(unix) {
            assert!(which("sh").is_some());
        }
        assert!(which("aispice-no-such-program-xyz").is_none());
    }
}
