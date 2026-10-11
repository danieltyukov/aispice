//! Finding and running KLayout and Magic.
//!
//! Each run gets a private temporary folder that is also its `HOME`,
//! `KLAYOUT_HOME` and `TMPDIR`, so neither program reads the user's or a
//! project's start-up files or shares temporary files with another run, and
//! runs from an argument vector with a timeout and
//! cancellation. Scripts are aispice's own files, written into the folder;
//! values reach them as `-rd name=value` arguments (KLayout) or environment
//! variables (Magic), never as script text.

use aispice_sim::backend::process::{self, ProcessOutput, ProcessSpec};
use aispice_sim::backend::which;
use aispice_sim::pdk::Pdk;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, thiserror::Error)]
pub enum LayoutError {
    #[error("{0} was not found; install it or set its path in the configuration")]
    NotFound(&'static str),
    #[error("the PDK `{pdk}` has no {what} ({path}){hint}")]
    PdkFiles {
        pdk: String,
        what: &'static str,
        path: String,
        hint: &'static str,
    },
    #[error("{0}")]
    Input(String),
    #[error("{program} failed: {message}")]
    Failed { program: String, message: String },
    #[error("{0}")]
    Io(String),
}

/// Where the layout programs are; found on `PATH` when unset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Programs {
    #[serde(default)]
    pub klayout: Option<PathBuf>,
    #[serde(default)]
    pub magic: Option<PathBuf>,
}

impl Programs {
    pub fn klayout(&self) -> Result<PathBuf, LayoutError> {
        match &self.klayout {
            Some(p) if p.is_file() => Ok(p.clone()),
            Some(_) => Err(LayoutError::NotFound("KLayout")),
            None => which("klayout").ok_or(LayoutError::NotFound("KLayout")),
        }
    }

    pub fn magic(&self) -> Result<PathBuf, LayoutError> {
        match &self.magic {
            Some(p) if p.is_file() => Ok(p.clone()),
            Some(_) => Err(LayoutError::NotFound("Magic")),
            None => which("magic").ok_or(LayoutError::NotFound("Magic")),
        }
    }
}

/// The files of a PDK's KLayout and Magic setup that aispice uses.
#[derive(Debug, Clone, Serialize)]
pub struct PdkLayoutFiles {
    pub pdk: String,
    pub drc_dir: PathBuf,
    pub drc_deck: PathBuf,
    pub drc_json: PathBuf,
    pub drc_json_default: PathBuf,
    pub lvs_deck: PathBuf,
    /// `libs.tech/klayout/python`, holding the pcell library.
    pub python: PathBuf,
    pub lyp: PathBuf,
    pub magicrc: PathBuf,
    /// Magic's tech file that keeps every GDS layer as drawn, for fill.
    pub magic_gds_tech: PathBuf,
    /// The PDK's folder, for its IO and standard-cell libraries.
    pub root: PathBuf,
    /// IHP's standard-cell netlist (CDL), for chip LVS.
    pub stdcell_cdl: PathBuf,
}

impl PdkLayoutFiles {
    /// The KLayout and Magic files of an IHP SG13G2 PDK. Only the decks
    /// are required here; the pcells and Magic are checked when used.
    pub fn of(pdk: &Pdk) -> Result<Self, LayoutError> {
        let tech = pdk.root.join("libs.tech/klayout/tech");
        let f = Self {
            pdk: pdk.name.clone(),
            drc_dir: tech.join("drc"),
            drc_deck: tech.join("drc/ihp-sg13g2.drc"),
            drc_json: pdk
                .root
                .join("libs.tech/klayout/python/sg13g2_pycell_lib/sg13g2_tech_mod.json"),
            drc_json_default: tech.join("drc/rule_decks/sg13g2_tech_default.json"),
            lvs_deck: tech.join("lvs/sg13g2.lvs"),
            python: pdk.root.join("libs.tech/klayout/python"),
            lyp: tech.join("sg13g2.lyp"),
            magicrc: pdk.root.join("libs.tech/magic/ihp-sg13g2.magicrc"),
            magic_gds_tech: pdk.root.join("libs.tech/magic/ihp-sg13g2-GDS.tech"),
            root: pdk.root.clone(),
            stdcell_cdl: pdk
                .root
                .join("libs.ref/sg13g2_stdcell/cdl/sg13g2_stdcell.cdl"),
        };
        for (what, path) in [
            ("KLayout DRC deck", &f.drc_deck),
            ("KLayout LVS deck", &f.lvs_deck),
            ("DRC rule values", &f.drc_json_default),
        ] {
            if !path.is_file() {
                return Err(f.missing(what, path, ""));
            }
        }
        Ok(f)
    }

    fn missing(&self, what: &'static str, path: &Path, hint: &'static str) -> LayoutError {
        LayoutError::PdkFiles {
            pdk: self.pdk.clone(),
            what,
            path: path.display().to_string(),
            hint,
        }
    }

    /// The pcell library needs two git submodules of IHP-Open-PDK.
    pub fn check_pcells(&self) -> Result<(), LayoutError> {
        let api = self.python.join("pycell4klayout-api/source/python/cni");
        let pre = self.python.join("pypreprocessor/pypreprocessor");
        for p in [&api, &pre] {
            if !p.is_dir() {
                return Err(self.missing(
                    "pcell support library",
                    p,
                    "; check out the submodules: git submodule update --init ihp-sg13g2/libs.tech/klayout/python/pycell4klayout-api ihp-sg13g2/libs.tech/klayout/python/pypreprocessor",
                ));
            }
        }
        Ok(())
    }

    pub fn check_magic(&self) -> Result<(), LayoutError> {
        if self.magicrc.is_file() {
            Ok(())
        } else {
            Err(self.missing("Magic start-up file", &self.magicrc, ""))
        }
    }
}

/// A private folder for one run of a layout program.
pub struct RunDir {
    dir: tempfile::TempDir,
}

impl RunDir {
    pub fn new() -> Result<Self, LayoutError> {
        let dir = tempfile::Builder::new()
            .prefix("aispice-layout-")
            .tempdir()
            .map_err(|e| LayoutError::Io(format!("cannot create a run folder: {e}")))?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Write one of aispice's scripts into the folder.
    pub fn script(&self, name: &str, text: &str) -> Result<PathBuf, LayoutError> {
        let p = self.path().join(name);
        std::fs::write(&p, text).map_err(|e| LayoutError::Io(format!("{}: {e}", p.display())))?;
        Ok(p)
    }
}

/// How long a layout program may run.
pub const TIMEOUT: Duration = Duration::from_secs(1800);

/// Run `klayout -b -r <script> -rd k=v ...` in `dir`.
pub async fn klayout(
    programs: &Programs,
    dir: &Path,
    script: &Path,
    vars: &[(&str, String)],
    cancel: &CancellationToken,
) -> Result<ProcessOutput, LayoutError> {
    let exe = programs.klayout()?;
    let mut spec = ProcessSpec::new(&exe, dir)
        .env("HOME", dir)
        .env("KLAYOUT_HOME", dir)
        // IHP's pcell library writes preprocessed modules under fixed names
        // in the temporary folder and deletes them after loading: a shared
        // /tmp lets two runs delete each other's files, or a local user
        // plant one. Each run gets its own.
        .env("TMPDIR", dir)
        .env("TMP", dir)
        .env("TEMP", dir)
        // Batch mode opens no window; this keeps Qt off the display too.
        .env("QT_QPA_PLATFORM", "offscreen")
        .arg("-b")
        .arg("-r")
        .arg(script);
    for (k, v) in vars {
        spec = spec.arg("-rd").arg(format!("{k}={v}"));
    }
    let out = process::run(&spec, TIMEOUT, cancel)
        .await
        .map_err(|e| LayoutError::Failed {
            program: "KLayout".into(),
            message: e.to_string(),
        })?;
    Ok(out)
}

/// Run Magic without graphics on a Tcl script of aispice's with a tech file
/// instead of a start-up file (fill uses the PDK's GDS tech file).
pub async fn magic_tech(
    programs: &Programs,
    dir: &Path,
    tech: &Path,
    script: &Path,
    env: &[(&str, String)],
    cancel: &CancellationToken,
) -> Result<ProcessOutput, LayoutError> {
    let exe = programs.magic()?;
    let mut spec = ProcessSpec::new(&exe, dir)
        .env("HOME", dir)
        .env("TMPDIR", dir)
        .env("MAGTYPE", "mag")
        .arg("-dnull")
        .arg("-noconsole")
        .arg("-T")
        .arg(tech)
        .arg(script);
    for (k, v) in env {
        spec = spec.env(*k, v);
    }
    process::run(&spec, TIMEOUT, cancel)
        .await
        .map_err(|e| LayoutError::Failed {
            program: "Magic".into(),
            message: e.to_string(),
        })
}

/// Run Magic without graphics on a Tcl script of aispice's, with the
/// PDK's start-up file and the given environment.
pub async fn magic(
    programs: &Programs,
    dir: &Path,
    rcfile: &Path,
    script: &Path,
    env: &[(&str, String)],
    cancel: &CancellationToken,
) -> Result<ProcessOutput, LayoutError> {
    let exe = programs.magic()?;
    let mut spec = ProcessSpec::new(&exe, dir)
        .env("HOME", dir)
        .env("TMPDIR", dir)
        .arg("-dnull")
        .arg("-noconsole")
        .arg("-rcfile")
        .arg(rcfile)
        .arg(script);
    for (k, v) in env {
        spec = spec.env(*k, v);
    }
    process::run(&spec, TIMEOUT, cancel)
        .await
        .map_err(|e| LayoutError::Failed {
            program: "Magic".into(),
            message: e.to_string(),
        })
}

/// The version a layout program reports (`klayout -v`, `magic --version`),
/// for evidence summaries; `unknown` when it does not say.
pub async fn version(programs: &Programs, magic: bool, cancel: &CancellationToken) -> String {
    let (exe, flag) = if magic {
        (programs.magic(), "--version")
    } else {
        (programs.klayout(), "-v")
    };
    let (Ok(exe), Ok(run)) = (exe, RunDir::new()) else {
        return "unknown".into();
    };
    let spec = ProcessSpec::new(&exe, run.path())
        .env("HOME", run.path())
        .env("KLAYOUT_HOME", run.path())
        .env("QT_QPA_PLATFORM", "offscreen")
        .arg(flag);
    match process::run(&spec, Duration::from_secs(60), cancel).await {
        Ok(out) => parse_version(&out.combined()).unwrap_or_else(|| "unknown".into()),
        Err(_) => "unknown".into(),
    }
}

/// The first word of a version banner that looks like a version number:
/// `KLayout 0.30.7` gives `0.30.7`, `8.3.624` gives itself.
pub fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|w| {
            w.contains('.')
                && w.starts_with(|c: char| c.is_ascii_digit())
                && w.chars().all(|c| c.is_ascii_digit() || c == '.')
        })
        .map(str::to_string)
}

/// The last `n` lines of a program's output, without KLayout's memory
/// usage lines.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.contains("Memory Usage"))
        .collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Whether a cell or net name is safe to hand to a layout program: letters,
/// digits and `_ $ . < > -`, at most 128 characters.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.' | '<' | '>' | '-'))
}

/// Refuse a name that is not [`safe_name`].
pub fn check_name(what: &str, name: &str) -> Result<(), LayoutError> {
    if safe_name(name) {
        Ok(())
    } else {
        Err(LayoutError::Input(format!(
            "{what} `{name}` has characters aispice does not pass to layout tools; use letters, digits and _ . $ < > -"
        )))
    }
}

/// Refuse a path a layout program would read or write that could be taken
/// for something else: control characters, or not absolute.
pub fn check_path(path: &Path) -> Result<(), LayoutError> {
    let s = path.to_string_lossy();
    if !path.is_absolute() || s.chars().any(char::is_control) {
        return Err(LayoutError::Input(format!(
            "{} is not a plain absolute path",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_reach_tools_are_plain() {
        for ok in ["ota5t", "TOP", "net$1", "bus<3>", "v-out", "a.b"] {
            assert!(safe_name(ok), "{ok}");
        }
        for bad in [
            "",
            "a b",
            "a;b",
            "[exec rm]",
            "a\"b",
            "a'b",
            "a\nb",
            "a{b}",
            &"x".repeat(129),
        ] {
            assert!(!safe_name(bad), "{bad}");
        }
        assert!(check_path(Path::new("relative.gds")).is_err());
        assert!(check_path(Path::new("/tmp/a\nb.gds")).is_err());
        assert!(check_path(&std::env::temp_dir().join("a.gds")).is_ok());
    }

    #[test]
    fn versions_are_read_from_banners() {
        assert_eq!(parse_version("KLayout 0.30.7\n").as_deref(), Some("0.30.7"));
        assert_eq!(parse_version("8.3.624").as_deref(), Some("8.3.624"));
        assert_eq!(
            parse_version("Magic 8.3 revision 624").as_deref(),
            Some("8.3")
        );
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn pdk_files_are_found_or_named() {
        let dir = tempfile::tempdir().unwrap();
        let root = aispice_sim::pdk::fake_ihp(dir.path());
        let pdk = Pdk::open("ihp", &root).unwrap();
        let err = PdkLayoutFiles::of(&pdk).unwrap_err().to_string();
        assert!(err.contains("KLayout DRC deck"), "{err}");
        for f in [
            "libs.tech/klayout/tech/drc/ihp-sg13g2.drc",
            "libs.tech/klayout/tech/drc/rule_decks/sg13g2_tech_default.json",
            "libs.tech/klayout/tech/lvs/sg13g2.lvs",
        ] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "").unwrap();
        }
        let files = PdkLayoutFiles::of(&pdk).unwrap();
        let err = files.check_pcells().unwrap_err().to_string();
        assert!(err.contains("git submodule update --init"), "{err}");
        assert!(files.check_magic().is_err());
    }
}
