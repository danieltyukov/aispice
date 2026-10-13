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
use crate::backend::native_code::TOOL_PATH;
use crate::backend::process::{self};
use crate::sandbox::{Limits, Plan, Profile, Sandbox};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::ffi::OsString;
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

/// Output points a run with behavioural libraries may ask for: the stop
/// time over the print step of its `.tran`. A co-simulated model runs at
/// every step, and the raw file grows with every point.
pub const MAX_RUN_POINTS: f64 = 2e6;
/// XSPICE instances (digital models, bridges, co-simulated models) one deck
/// may have.
pub const MAX_XSPICE_INSTANCES: usize = 4096;

/// Refuse a deck whose run would be too long to co-simulate, before
/// anything is built: a `.tran` over [`MAX_RUN_POINTS`] points (saying the
/// longest stop time its step allows) or more than
/// [`MAX_XSPICE_INSTANCES`] XSPICE instances.
pub fn check_run_size(netlist: &aispice_core::netlist::Netlist) -> Result<(), String> {
    use aispice_core::netlist::Line;
    fn walk(items: &[Line], a: &mut usize, tran: &mut Vec<String>) {
        for item in items {
            match item {
                Line::Element(e) if e.letter() == 'A' => *a += 1,
                Line::Directive { text }
                    if text.trim_start().to_ascii_lowercase().starts_with(".tran") =>
                {
                    tran.push(text.clone())
                }
                Line::Subckt(s) => walk(&s.body, a, tran),
                _ => {}
            }
        }
    }
    let mut instances = 0usize;
    let mut trans = Vec::new();
    walk(&netlist.items, &mut instances, &mut trans);
    if instances > MAX_XSPICE_INSTANCES {
        return Err(format!(
            "the deck has {instances} XSPICE instances; a run with behavioural models may have at most {MAX_XSPICE_INSTANCES}"
        ));
    }
    for t in trans {
        // Times only as plain numbers: an expression (`{st}`) could hide a
        // step this check cannot weigh.
        let raw: Vec<&str> = t
            .split_whitespace()
            .skip(1)
            .filter(|w| !w.eq_ignore_ascii_case("uic"))
            .collect();
        let words: Vec<f64> = raw
            .iter()
            .map_while(|w| aispice_core::units::parse(w))
            .collect();
        if words.len() < raw.len().min(4) {
            return Err(format!(
                "`{}`: in a run with behavioural models, write the .tran times as plain numbers",
                t.trim()
            ));
        }
        let (step, stop) = match words[..] {
            [stop] => (0.0, stop),
            [step, stop, ..] => (step, stop),
            _ => continue,
        };
        if !stop.is_finite() {
            return Err(format!("`{}` has no finite stop time", t.trim()));
        }
        // ngspice prints at least every `step`, whatever its internal steps.
        // A zero step is LTspice's "choose one", which the translation
        // turns into the maximum step when one is given, else a thousandth
        // of the run: the same here.
        let step = if step > 0.0 {
            step
        } else {
            match words.get(3).copied().filter(|m| *m > 0.0) {
                Some(tmax) => tmax,
                None => continue,
            }
        };
        let points = stop / step;
        if points > MAX_RUN_POINTS {
            return Err(format!(
                "`{}` asks for {points:.3e} output points; a run with behavioural models may have at most {MAX_RUN_POINTS:.0e}: with a step of {} the stop time can be at most {}",
                t.trim(),
                aispice_core::units::format(step),
                aispice_core::units::format(step * MAX_RUN_POINTS)
            ));
        }
    }
    Ok(())
}

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

/// System folders a compiler toolchain runs from: the compiler driver, the
/// assembler and linker it spawns, its helper programs and `make`. Granting
/// execute on them lets the whole toolchain run while the untrusted sources
/// and anything compiled from them stay in the writable build folder, which
/// has no execute right.
const TOOLCHAIN: &[&str] = &["/usr", "/bin", "/sbin", "/lib", "/lib64", "/usr/local"];

/// The largest file a build step may write (`RLIMIT_FSIZE`): a Verilator
/// build or an OSDI is a few megabytes; this is generous.
const BUILD_MAX_FILE: u64 = 256 << 20;
/// A build step's address space (`RLIMIT_AS`): compilers are memory-hungry.
const BUILD_MAX_MEMORY: u64 = 6 << 30;

/// Where a compiler aispice runs may reach, besides the program and the
/// step's own reads: the tools come only from aispice's trusted path, so no
/// project folder a `PATH` names can supply one. The sandbox that runs each
/// step, and the system folders it already shows, come from [`Sandbox`].
#[derive(Debug, Clone)]
pub struct Jail {
    sandbox: Sandbox,
}

impl Jail {
    pub fn new(sandbox: Sandbox) -> Self {
        Self { sandbox }
    }

    /// A C++ compiler for Verilator's link step, from the trusted path only.
    pub fn cxx(&self) -> Option<PathBuf> {
        tool(&["g++", "c++", "clang++"])
    }

    /// `make`, which Verilator runs, from the trusted path only.
    pub fn make(&self) -> Option<PathBuf> {
        tool(&["make", "gmake"])
    }
}

/// The first of `names` found in [`TOOL_PATH`], aispice's trusted tool path.
pub fn tool(names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|n| {
        TOOL_PATH
            .iter()
            .map(|d| Path::new(d).join(n))
            .find(|p| crate::backend::is_executable(p))
    })
}

/// The fixed environment a build step runs with: nothing of aispice's own
/// (no API keys, `CFLAGS`, `MAKEFLAGS`, `CPATH` or the like), the trusted
/// path so a tool the compiler spawns comes from there, and the build
/// folder as `HOME` and `TMPDIR`.
fn build_env(dir: &Path) -> Vec<(OsString, OsString)> {
    vec![
        ("PATH".into(), TOOL_PATH.join(":").into()),
        ("HOME".into(), dir.into()),
        ("TMPDIR".into(), dir.into()),
        ("LC_ALL".into(), "C".into()),
    ]
}

/// The sandbox plan for one build step: `program` may read the system, the
/// toolchain, `dir` and `reads`; execute the toolchain and `program`; write
/// only `dir`; and starts from the fixed [`build_env`]. The seccomp denylist
/// leaves a compiler its ordinary calls while refusing sockets, tracing,
/// namespaces and the like.
fn build_plan(program: &Path, args: &[String], dir: &Path, reads: &[PathBuf]) -> Plan {
    // Only real directories: on a merged-`/usr` system `/bin`, `/lib` and
    // the rest are symlinks into `/usr`, which the sandbox will not open as
    // a rule (and `/usr` already covers them); on a split system they are
    // real folders that must be granted.
    let toolchain: Vec<PathBuf> = TOOLCHAIN
        .iter()
        .map(PathBuf::from)
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
        .collect();
    let mut read = reads.to_vec();
    read.extend(toolchain.iter().cloned());
    let mut exec = toolchain;
    exec.push(program.to_path_buf());
    Plan {
        program: program.to_path_buf(),
        args: args.iter().map(OsString::from).collect(),
        env: build_env(dir),
        cwd: dir.to_path_buf(),
        read,
        exec,
        write: vec![dir.to_path_buf()],
        profile: Profile::Denylist,
        limits: Limits {
            file_size: Some(BUILD_MAX_FILE),
            address_space: Some(BUILD_MAX_MEMORY),
            nproc_headroom: Some(256),
        },
    }
}

/// Run one build step for `program` in the build folder `dir`, confined by
/// `jail` ([`build_plan`]). Runs within what is left of `deadline`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn step(
    jail: &Jail,
    program: &Path,
    args: &[String],
    dir: &Path,
    reads: &[PathBuf],
    deadline: std::time::Instant,
    cancel: &CancellationToken,
) -> Result<process::ProcessOutput, BuildError> {
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        return Err(BuildError::Refused("the build took too long".into()));
    }
    let plan = build_plan(program, args, dir, reads);
    let spec = jail
        .sandbox
        .command(&plan)
        .map_err(|e| BuildError::Io(e.to_string()))?;
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

/// A program's version line, from `--version`, within ten seconds. This
/// runs the trusted compiler itself on no input, so it needs no sandbox,
/// but it still starts from a fixed environment (nothing of aispice's, the
/// trusted path) so it matches a build and leaks no variable.
pub async fn version(program: &Path) -> Option<String> {
    let dir = build_dir().ok()?;
    let mut spec = process::ProcessSpec::new(program, dir.path());
    spec.args = vec![OsString::from("--version")];
    spec.clear_env = true;
    spec.env = build_env(dir.path());
    let out = process::run(&spec, Duration::from_secs(10), &CancellationToken::new())
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
    /// The C compiler that builds the library a sandboxed ngspice
    /// preloads ([`crate::backend::native_code`]).
    pub cc: Option<PathBuf>,
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
            cc: crate::backend::native_code::compiler(),
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

    /// A build step may write only its build folder, reads the system, the
    /// toolchain, the build folder and the step's own reads (never the
    /// project), executes only the toolchain and the compiler, runs under
    /// the seccomp denylist, and starts from a fixed environment with
    /// nothing of aispice's own and the trusted path.
    #[cfg(unix)]
    #[test]
    fn a_build_step_is_confined_to_the_build_folder_and_the_toolchain() {
        let dir = Path::new("/tmp/aispice-build-xyz");
        let shim = PathBuf::from("/opt/ngspice/share/ngspice/scripts/src");
        let cc = PathBuf::from("/opt/openvaf/bin/openvaf");
        let reads = [shim.clone()];
        let plan = build_plan(&cc, &["-o".into(), "x.osdi".into()], dir, &reads);
        assert_eq!(plan.profile, Profile::Denylist);
        assert_eq!(plan.write, vec![dir.to_path_buf()]);
        assert_eq!(plan.cwd, dir.to_path_buf());
        assert!(plan.read.contains(&shim), "{:?}", plan.read);
        assert!(plan.exec.contains(&cc), "{:?}", plan.exec);
        // The toolchain folders present on this machine are read and
        // executable; `/usr` is on every supported machine.
        assert!(plan.read.contains(&PathBuf::from("/usr")));
        assert!(plan.exec.contains(&PathBuf::from("/usr")));
        // A merged-/usr symlink such as `/bin` is never a rule path.
        for p in &plan.read {
            assert!(
                !std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()),
                "{p:?} is a symlink"
            );
        }
        // Nothing of the environment but the fixed five.
        let keys: Vec<_> = plan.env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        assert_eq!(keys, ["PATH", "HOME", "TMPDIR", "LC_ALL"]);
        let path = &plan.env[0].1;
        assert_eq!(path, "/usr/local/bin:/usr/bin:/bin");
        assert_eq!(plan.env[1].1, dir);
        assert!(plan.limits.file_size.is_some() && plan.limits.nproc_headroom.is_some());
    }

    #[test]
    fn the_build_tools_come_only_from_the_trusted_path() {
        for name in ["g++", "c++", "clang++", "make", "verilator", "openvaf"] {
            if let Some(p) = tool(&[name]) {
                let dir = p.parent().unwrap().to_string_lossy().into_owned();
                assert!(
                    TOOL_PATH.contains(&dir.as_str()),
                    "{name} resolved outside the trusted path: {p:?}"
                );
            }
        }
    }

    #[test]
    fn long_or_wide_runs_are_refused_before_anything_builds() {
        use aispice_core::netlist::parse;
        assert!(check_run_size(&parse("t\nV1 a 0 1\n.tran 1n 1m\n.end\n")).is_ok());
        let e = check_run_size(&parse("t\nV1 a 0 1\n.tran 1p 1\n.end\n")).unwrap_err();
        assert!(
            e.contains("at most 2e6") && e.contains("the stop time can be at most 2u"),
            "{e}"
        );
        assert!(check_run_size(&parse("t\n.tran 0 1m\n.end\n")).is_ok());
        // A zero step with a maximum step prints at the maximum step.
        assert!(check_run_size(&parse("t\n.tran 0 1 0 1p\n.end\n")).is_err());
        let e = check_run_size(&parse("t\n.param st=1p\n.tran {st} 1\n.end\n")).unwrap_err();
        assert!(e.contains("plain numbers"), "{e}");
        assert!(check_run_size(&parse("t\n.tran 1n 1u uic\n.end\n")).is_ok());
        let many: String = (0..=MAX_XSPICE_INSTANCES)
            .map(|i| format!("A{i} [a] [b] buf\n"))
            .collect();
        let e = check_run_size(&parse(&format!("t\n{many}.tran 1n 1u\n.end\n"))).unwrap_err();
        assert!(e.contains("4097 XSPICE instances"), "{e}");
    }

    #[test]
    fn names_and_logs() {
        assert_eq!(safe_name("counter"), "counter");
        assert_eq!(safe_name("my-model.v"), "my_model_v");
        assert_eq!(safe_name("4bit"), "m4bit");
        assert_eq!(safe_name(""), "model");
        // The compiler names files as the system writes paths.
        let dir = std::env::temp_dir().join("aispice-build-x");
        let log = format!(
            "\u{1b}[1m\u{1b}[31merror\u{1b}[0m: bad\n  --> {}:3:4\n\n {}/obj/x.h",
            dir.join("comparator.va").display(),
            dir.display()
        );
        assert_eq!(
            clean_log(
                &log,
                &dir,
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
