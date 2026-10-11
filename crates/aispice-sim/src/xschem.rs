//! Running xschem as a netlister, to check a conversion against xschem's own
//! netlist.
//!
//! xschem evaluates Tcl from the files it opens and splices file names into
//! Tcl commands, so it is only run on a schematic that
//! [`aispice_core::xschem::safety::scan`] accepted, and only like this:
//!
//! - from an argument vector, with stdin closed, a timeout and capped
//!   output, headless (`-x`: no window, `-q`: quit when done);
//! - with an empty environment except `PATH` (xschem runs `awk`), and a
//!   `HOME` that is a fresh private folder, so the user's
//!   `~/.xschem/xschemrc` is never read;
//! - with `--rcfile` pointing at a fixed rc aispice writes into that folder,
//!   which reads the library path, the output folder and the model folders
//!   from environment variables, so no path is ever spliced into Tcl text;
//! - only when every path xschem sees (the schematic, the library folders,
//!   every file the schematic pulls in, the model folders) consists of
//!   letters, digits and `/ . _ - + , = @`.

use crate::backend::process::{self, ProcessSpec};
use crate::backend::{SimError, which};
use aispice_core::xschem::safety::safe_path;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A netlist run takes about a tenth of a second; this is generous.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Netlists larger than this are not read.
const MAX_NETLIST: u64 = 16 << 20;

/// The rc file every run uses. It never contains a path: those come from
/// the environment.
pub const RC: &str = "\
# Written by aispice for one headless netlist run.
set XSCHEM_LIBRARY_PATH $env(AISPICE_XSCHEM_LIBRARY_PATH)
set netlist_dir $env(AISPICE_XSCHEM_NETLIST_DIR)
if {[info exists env(AISPICE_MODELS_NGSPICE)]} {
  set MODELS_NGSPICE $env(AISPICE_MODELS_NGSPICE)
  set SG13G2_MODELS $env(AISPICE_MODELS_NGSPICE)
}
if {[info exists env(AISPICE_MODELS_XYCE)]} {
  set MODELS_XYCE $env(AISPICE_MODELS_XYCE)
  set SG13G2_MODELS_XYCE $env(AISPICE_MODELS_XYCE)
}
";

/// An xschem installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xschem {
    pub program: PathBuf,
    /// xschem's shared folder, holding `xschem_library`.
    pub share: Option<PathBuf>,
}

impl Xschem {
    /// `AISPICE_XSCHEM`, else `xschem` on `PATH`. The shared folder is
    /// `AISPICE_XSCHEM_SHAREDIR`, else `share/xschem` beside the program's
    /// `bin` folder.
    pub fn find() -> Option<Self> {
        let program = match std::env::var_os("AISPICE_XSCHEM") {
            Some(p) => {
                let p = PathBuf::from(p);
                p.is_file().then_some(p)?
            }
            None => which("xschem")?,
        };
        let share = std::env::var_os("AISPICE_XSCHEM_SHAREDIR")
            .map(PathBuf::from)
            .or_else(|| {
                let real = program.canonicalize().ok()?;
                let prefix = real.parent()?.parent()?;
                Some(prefix.join("share/xschem"))
            })
            .filter(|d| d.join("xschem_library").is_dir());
        Some(Self { program, share })
    }

    /// xschem's own symbol library, in its search order: `devices`, then
    /// the library root.
    pub fn library(&self) -> Vec<PathBuf> {
        match &self.share {
            Some(s) => vec![s.join("xschem_library/devices"), s.join("xschem_library")],
            None => Vec::new(),
        }
    }
}

/// What one run reads.
#[derive(Debug, Clone, Default)]
pub struct Run {
    /// `XSCHEM_LIBRARY_PATH`, in order.
    pub library: Vec<PathBuf>,
    /// Every file the schematic pulls in (from the safety scan), checked
    /// for characters xschem would read as Tcl.
    pub files: Vec<PathBuf>,
    pub models_ngspice: Option<PathBuf>,
    pub models_xyce: Option<PathBuf>,
}

/// Paths xschem would see that it must not: their characters could be read
/// as Tcl.
pub fn unsafe_paths(sch: &Path, run: &Run) -> Vec<PathBuf> {
    std::iter::once(sch)
        .chain(run.library.iter().map(PathBuf::as_path))
        .chain(run.files.iter().map(PathBuf::as_path))
        .chain(run.models_ngspice.as_deref())
        .chain(run.models_xyce.as_deref())
        .filter(|p| !safe_path(p))
        .map(Path::to_path_buf)
        .collect()
}

/// Netlist `sch` with xschem and return the netlist text.
pub async fn netlist(
    x: &Xschem,
    sch: &Path,
    run: &Run,
    cancel: &CancellationToken,
) -> Result<String, SimError> {
    let sch = sch
        .canonicalize()
        .map_err(|e| SimError::Io(format!("{}: {e}", sch.display())))?;
    let bad = unsafe_paths(&sch, run);
    if let Some(p) = bad.first() {
        return Err(SimError::Config(format!(
            "xschem was not run: the path {} has characters xschem would read as Tcl (only letters, digits and / . _ - + , = @ are passed to it)",
            p.display()
        )));
    }
    let tmp = tempfile::tempdir().map_err(|e| SimError::Io(e.to_string()))?;
    let home = tmp.path().join("home");
    let out = tmp.path().join("out");
    for d in [&home, &out] {
        std::fs::create_dir(d).map_err(|e| SimError::Io(e.to_string()))?;
    }
    let rc = tmp.path().join("aispicerc.tcl");
    std::fs::write(&rc, RC).map_err(|e| SimError::Io(e.to_string()))?;
    let joined = run
        .library
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    let mut spec = ProcessSpec::new(&x.program, &out)
        .arg("-n")
        .arg("-s")
        .arg("-q")
        .arg("-x")
        .arg("-r")
        .arg("--rcfile")
        .arg(&rc)
        .arg("-o")
        .arg(&out)
        .arg("-N")
        .arg("netlist.spice")
        .arg(&sch)
        .env("HOME", &home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("LC_ALL", "C")
        .env("AISPICE_XSCHEM_LIBRARY_PATH", joined)
        .env("AISPICE_XSCHEM_NETLIST_DIR", &out);
    if let Some(m) = &run.models_ngspice {
        spec = spec.env("AISPICE_MODELS_NGSPICE", m);
    }
    if let Some(m) = &run.models_xyce {
        spec = spec.env("AISPICE_MODELS_XYCE", m);
    }
    spec.clear_env = true;
    let result = process::run(&spec, TIMEOUT, cancel).await?;
    let path = out.join("netlist.spice");
    let size = std::fs::metadata(&path).map(|m| m.len()).map_err(|_| {
        SimError::NoOutput(format!(
            "xschem wrote no netlist{}",
            first_line(&result.combined())
                .map(|l| format!(": {l}"))
                .unwrap_or_default()
        ))
    })?;
    if size > MAX_NETLIST {
        return Err(SimError::TooLarge {
            what: "xschem's netlist".into(),
            size,
            limit: MAX_NETLIST,
        });
    }
    std::fs::read_to_string(&path).map_err(|e| SimError::Io(e.to_string()))
}

fn first_line(log: &str) -> Option<String> {
    log.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rc_holds_no_path() {
        assert!(!RC.contains('/'));
        assert!(RC.contains("$env(AISPICE_XSCHEM_LIBRARY_PATH)"));
    }

    #[test]
    fn paths_with_tcl_characters_are_refused_before_running() {
        let run = Run {
            library: vec![PathBuf::from("/usr/share/xschem/xschem_library")],
            files: vec![PathBuf::from("/tmp/a [x].sym")],
            ..Run::default()
        };
        let bad = unsafe_paths(Path::new("/tmp/tb.sch"), &run);
        assert_eq!(bad, [PathBuf::from("/tmp/a [x].sym")]);
        assert!(unsafe_paths(Path::new("/tmp/$x.sch"), &Run::default()).len() == 1);
    }

    #[tokio::test]
    async fn unsafe_schematic_path_never_starts_xschem() {
        let tmp = tempfile::tempdir().unwrap();
        let sch = tmp.path().join("a b.sch");
        std::fs::write(&sch, "v {xschem version=3.4.4 file_version=1.2}\n").unwrap();
        // A program that does not exist: the refusal comes first.
        let x = Xschem {
            program: tmp.path().join("no-xschem"),
            share: None,
        };
        let err = netlist(&x, &sch, &Run::default(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("xschem was not run"), "{err}");
    }

    /// Gated on xschem being installed: a small schematic netlists, with no
    /// window and without reading the user's rc.
    #[tokio::test]
    async fn netlists_headless() {
        let Some(x) = Xschem::find() else {
            return;
        };
        if x.share.is_none() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let sch = dir.join("rc.sch");
        std::fs::write(
            &sch,
            "v {xschem version=3.4.4 file_version=1.2}\nN 0 -30 0 -60 {}\nC {res.sym} 0 0 0 0 {name=R1 value=1k}\nC {gnd.sym} 0 30 0 0 {name=l1 lab=GND}\nC {lab_pin.sym} 0 -60 0 0 {name=p1 lab=out}\n",
        )
        .unwrap();
        let run = Run {
            library: x.library(),
            ..Run::default()
        };
        let text = netlist(&x, &sch, &run, &CancellationToken::new())
            .await
            .unwrap();
        assert!(text.contains("R1 out GND 1k m=1"), "{text}");
    }
}
