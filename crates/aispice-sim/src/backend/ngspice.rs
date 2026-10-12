//! ngspice in batch mode.
//!
//! `ngspice -b -r deck.raw deck.cir`, with HOME pointed at the private run
//! directory and aispice's own `.spiceinit` there: ngspice executes the
//! `.spiceinit` it finds in HOME and in the working directory, and those are
//! the only two places it looks, so neither the user's nor a project's file
//! can run. The one we write turns on LTspice compatibility
//! (`ngbehavior=ltpsa`), which aispice's netlists rely on.
//!
//! A PDK run differs in two ways. The `.spiceinit` loads the PDK's compiled
//! device models (`osdi '<file>'`), the one thing a deck may never do. And
//! it leaves compatibility off: in ngspice 42 the LTspice and PSpice modes
//! turn `.lib file section` into `.include file section`, which PDK corner
//! libraries are built on, and even with the sections inlined IHP's PSP
//! models then give wrong currents (an inverter's output at 0.75 V instead
//! of 1.2 V with its input low).
//!
//! Behavioural libraries aispice built (Verilog-A as OSDI, Verilog for
//! XSPICE `d_cosim`) are written into the same private folder from the bytes
//! aispice holds, checked against their SHA-256, and loaded from there: OSDI
//! files by `osdi` lines in the `.spiceinit`, co-simulation libraries by
//! `d_cosim` model cards added to the deck after the netlist policy checked
//! it. No path a deck or a project names is ever loaded.
//!
//! ngspice refuses to evaluate `.meas` in batch mode when it writes a raw
//! file, so a deck with measurements runs twice: once for the raw file and
//! once, without it, for the measurements. ngspice runs take milliseconds;
//! the job's timeout covers both.

use super::process::{self, ProcessSpec};
use super::{
    Detection, SimError, SimId, SimJob, SimOutput, Simulator, Workspace, finish, read_raw_limited,
    which,
};
use crate::behavioral::{Library, LibraryKind, sha256_hex};
use crate::log::parse_ngspice_output;
use crate::pdk::PdkRun;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct Ngspice {
    /// The executable; found on PATH when unset.
    pub exe: Option<PathBuf>,
    /// The `ngbehavior` compatibility setting. `ltpsa` matches LTspice;
    /// `None` runs plain ngspice.
    pub compat: Option<String>,
}

impl Default for Ngspice {
    fn default() -> Self {
        Self {
            exe: None,
            compat: Some("ltpsa".into()),
        }
    }
}

impl Ngspice {
    fn exe(&self) -> Option<PathBuf> {
        match &self.exe {
            Some(p) => p.is_file().then(|| p.clone()),
            // On Windows `ngspice.exe` is the GUI build and `ngspice_con.exe`
            // the console one that batch runs need.
            None => {
                if cfg!(windows) {
                    which("ngspice_con").or_else(|| which("ngspice"))
                } else {
                    which("ngspice")
                }
            }
        }
    }

    /// The `.spiceinit` aispice runs ngspice with.
    pub fn spiceinit(&self) -> String {
        self.spiceinit_for(None)
    }

    /// The `.spiceinit` for a run, loading a PDK's OSDI files when it has
    /// one. The paths were checked by the caller; a quote or a newline would
    /// end the command early, so such a path is left out rather than
    /// written (the run then fails on the missing model, visibly).
    pub fn spiceinit_for(&self, pdk: Option<&PdkRun>) -> String {
        self.spiceinit_with(pdk, &[])
    }

    /// As [`spiceinit_for`](Self::spiceinit_for), also loading behavioural
    /// OSDI libraries aispice wrote into the run folder. They load in either
    /// mode, so a board-level run keeps its LTspice compatibility.
    pub fn spiceinit_with(&self, pdk: Option<&PdkRun>, osdi: &[PathBuf]) -> String {
        let mut s = String::from("* written by aispice for this run\n");
        let mut load = |f: &Path| {
            let p = f.display().to_string();
            if plain_path(&p) {
                s.push_str(&format!("osdi '{p}'\n"));
            }
        };
        match pdk {
            Some(pdk) => {
                for f in &pdk.osdi {
                    load(f);
                }
                for f in osdi {
                    load(f);
                }
            }
            None => {
                for f in osdi {
                    load(f);
                }
                if let Some(c) = &self.compat {
                    s.push_str(&format!("set ngbehavior={c}\n"));
                }
            }
        }
        s
    }

    /// The argument vector for a deck named `stem`.
    pub fn args(stem: &str) -> Vec<String> {
        vec![
            "-b".into(),
            "-r".into(),
            format!("{stem}.raw"),
            format!("{stem}.cir"),
        ]
    }

    /// The second run that evaluates `.meas` lines.
    pub fn meas_args(stem: &str) -> Vec<String> {
        vec!["-b".into(), format!("{stem}_meas.cir")]
    }
}

/// Whether a path can be written into a `.spiceinit` command or a quoted
/// model parameter as it is.
fn plain_path(p: &str) -> bool {
    !p.contains(['\'', '"', '$', '`', '\\']) && !p.chars().any(char::is_control)
}

/// Write each library into the run folder from aispice's bytes and check
/// the file holds exactly them before anything loads it.
fn place_libraries(
    ws: &Workspace,
    libraries: &[Arc<Library>],
) -> Result<Vec<(Arc<Library>, PathBuf)>, SimError> {
    let mut out = Vec::new();
    for lib in libraries {
        let path = ws.write(&lib.file_name(), &lib.bytes)?;
        let written = std::fs::read(&path)
            .map_err(|e| SimError::Io(format!("cannot read back {}: {e}", lib.file_name())))?;
        if sha256_hex(&written) != lib.sha256 || sha256_hex(&lib.bytes) != lib.sha256 {
            return Err(SimError::Config(format!(
                "the {} library aispice built does not match its recorded SHA-256; it was not loaded",
                lib.name
            )));
        }
        if !plain_path(&path.display().to_string()) {
            return Err(SimError::Config(format!(
                "the temporary folder {} has a quote, `$`, backslash or control character in its path, so a library cannot be loaded from it; set TMPDIR to a plain path",
                ws.path().display()
            )));
        }
        out.push((lib.clone(), path));
    }
    Ok(out)
}

/// The deck with a `d_cosim` model card for each co-simulation library,
/// before its `.end`: the only place a `d_cosim` model can come from, since
/// the netlist policy refuses them in decks. The library is named relative
/// to the run folder ngspice runs in: ngspice lower-cases a card, the path
/// in quotes included, and the private folder's own name has capitals.
pub fn with_cosim_models(deck: &str, cosim: &[(String, String)]) -> String {
    if cosim.is_empty() {
        return deck.to_string();
    }
    let lines: Vec<&str> = deck.lines().collect();
    let end = lines
        .iter()
        .rposition(|l| l.trim().eq_ignore_ascii_case(".end"))
        .unwrap_or(lines.len());
    let mut out: Vec<String> = lines[..end].iter().map(|l| l.to_string()).collect();
    out.push("* co-simulation models added by aispice".into());
    for (model, file) in cosim {
        out.push(format!(".model {model} d_cosim simulation=\"./{file}\""));
    }
    out.extend(lines[end..].iter().map(|l| l.to_string()));
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// The deck for the measurement run. In batch mode ngspice keeps only the
/// vectors it can read out of the `.meas` lines, and it cannot read
/// `vm(out)` or `vdb(out)`, so an AC run measuring those would save nothing
/// and be skipped; `.save all` keeps every node.
fn measurement_deck(deck: &str) -> String {
    let lines: Vec<&str> = deck.lines().collect();
    let end = lines
        .iter()
        .rposition(|l| l.trim().eq_ignore_ascii_case(".end"))
        .unwrap_or(lines.len());
    let mut out: Vec<&str> = lines[..end].to_vec();
    out.push(".save all");
    out.extend_from_slice(&lines[end..]);
    let mut s = out.join("\n");
    s.push('\n');
    s
}

/// Whether the deck has `.meas` or `.measure` lines.
fn has_meas(deck: &str) -> bool {
    deck.lines().any(|l| {
        let w = l.split_whitespace().next().unwrap_or("");
        w.eq_ignore_ascii_case(".meas") || w.eq_ignore_ascii_case(".measure")
    })
}

#[async_trait::async_trait]
impl Simulator for Ngspice {
    fn id(&self) -> SimId {
        SimId::Ngspice
    }

    async fn detect(&self) -> Detection {
        let Some(exe) = self.exe() else {
            return Detection {
                notes: vec!["ngspice is not on PATH".into()],
                ..Default::default()
            };
        };
        let mut d = Detection {
            found: true,
            path: Some(exe.clone()),
            ..Default::default()
        };
        if let Ok(ws) = Workspace::new() {
            let spec = ProcessSpec::new(&exe, ws.path())
                .arg("--version")
                .env("HOME", ws.path())
                .env("USERPROFILE", ws.path());
            if let Ok(out) =
                process::run(&spec, Duration::from_secs(10), &CancellationToken::new()).await
            {
                d.version = out.combined().lines().find_map(|l| {
                    let i = l.find("ngspice-")?;
                    l[i + 8..].split_whitespace().next().map(str::to_string)
                });
            }
        }
        if let Some(c) = &self.compat {
            d.notes.push(format!("runs with ngbehavior={c}"));
        }
        d
    }

    async fn run(&self, job: &SimJob, cancel: &CancellationToken) -> Result<SimOutput, SimError> {
        let exe = self
            .exe()
            .ok_or_else(|| SimError::NotFound("ngspice".into()))?;
        let stem = job.stem();
        let ws = Workspace::new()?;
        let placed = place_libraries(&ws, &job.libraries)?;
        let osdi: Vec<PathBuf> = placed
            .iter()
            .filter(|(l, _)| l.kind == LibraryKind::Osdi)
            .map(|(_, p)| p.clone())
            .collect();
        let cosim: Vec<(String, String)> = placed
            .iter()
            .filter(|(l, _)| l.kind == LibraryKind::Cosim)
            .map(|(l, _)| (l.name.clone(), l.file_name()))
            .collect();
        let deck = with_cosim_models(&job.netlist_text, &cosim);
        ws.write(&format!("{stem}.cir"), deck.as_bytes())?;
        ws.write(
            ".spiceinit",
            self.spiceinit_with(job.pdk.as_ref(), &osdi).as_bytes(),
        )?;
        let mut spec = ProcessSpec::new(&exe, ws.path())
            .env("HOME", ws.path())
            .env("USERPROFILE", ws.path());
        for a in Self::args(&stem) {
            spec = spec.arg(a);
        }
        let started = std::time::Instant::now();
        let out = process::run(&spec, job.timeout, cancel).await?;
        let mut log = out.combined();
        let mut report = parse_ngspice_output(&log);
        if has_meas(&deck) && report.errors.is_empty() {
            // ngspice will not evaluate .meas in batch mode when writing a
            // raw file, so the measurements come from a second run without
            // one, within the same deadline.
            let left = job.timeout.saturating_sub(started.elapsed());
            ws.write(
                &format!("{stem}_meas.cir"),
                measurement_deck(&deck).as_bytes(),
            )?;
            let mut spec2 = ProcessSpec::new(&exe, ws.path())
                .env("HOME", ws.path())
                .env("USERPROFILE", ws.path());
            for a in Self::meas_args(&stem) {
                spec2 = spec2.arg(a);
            }
            let m = process::run(&spec2, left, cancel).await?;
            let mlog = m.combined();
            let mreport = parse_ngspice_output(&mlog);
            report.measurements = mreport.measurements;
            for w in mreport.warnings {
                if !report.warnings.contains(&w) {
                    report.warnings.push(w);
                }
            }
            log.push_str("\n[aispice: measurement run]\n");
            log.push_str(&mlog);
        }
        let raw = ws.file(&format!("{stem}.raw"));
        let datasets = if raw.exists() {
            read_raw_limited(&raw)?
        } else {
            Vec::new()
        };
        let files = ws.keep(
            &job.run_dir,
            &[format!("{stem}.cir"), format!("{stem}.raw")],
        )?;
        let raw_path = files
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "raw"))
            .cloned();
        let result = SimOutput {
            datasets,
            log,
            measurements: report.measurements,
            errors: report.errors,
            warnings: report.warnings,
            duration: out.elapsed,
            raw_path,
            files,
        };
        finish(result, out.success(), "ngspice wrote no results")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_and_spiceinit() {
        assert_eq!(Ngspice::args("rc"), vec!["-b", "-r", "rc.raw", "rc.cir"]);
        assert_eq!(Ngspice::meas_args("rc"), vec!["-b", "rc_meas.cir"]);
        assert_eq!(
            measurement_deck("t\nR1 a 0 1\n.meas ac x FIND vm(a) AT=1k\n.end\n"),
            "t\nR1 a 0 1\n.meas ac x FIND vm(a) AT=1k\n.save all\n.end\n"
        );
        assert_eq!(
            measurement_deck("t\nR1 a 0 1\n"),
            "t\nR1 a 0 1\n.save all\n"
        );
        assert!(has_meas("t\nR1 a 0 1\n  .MEAS tran x MAX V(a)\n"));
        assert!(!has_meas("t\nR1 a 0 1\n* .meas in a comment\n"));
        assert!(
            Ngspice::default()
                .spiceinit()
                .contains("set ngbehavior=ltpsa\n")
        );
        let plain = Ngspice {
            compat: None,
            ..Default::default()
        };
        assert!(!plain.spiceinit().contains("ngbehavior"));
    }

    #[test]
    fn pdk_runs_load_their_osdi_files_in_native_mode() {
        let pdk = PdkRun {
            osdi: vec![
                PathBuf::from("/pdk/osdi/psp103.osdi"),
                PathBuf::from("/pdk/osdi/r3_cmc.osdi"),
                PathBuf::from("/pdk/it's/evil.osdi"),
                PathBuf::from("/pdk/x\nshell id/evil.osdi"),
            ],
            xyce_plugins: Vec::new(),
        };
        let init = Ngspice::default().spiceinit_for(Some(&pdk));
        assert_eq!(
            init,
            "* written by aispice for this run\nosdi '/pdk/osdi/psp103.osdi'\nosdi '/pdk/osdi/r3_cmc.osdi'\n"
        );
        let plain = Ngspice::default().spiceinit_for(None);
        assert!(!plain.contains("osdi") && plain.contains("ngbehavior=ltpsa"));
    }

    #[test]
    fn behavioural_libraries_load_from_the_run_folder_only() {
        let va = vec![PathBuf::from("/tmp/aispice-run-x/comparator.osdi")];
        let board = Ngspice::default().spiceinit_with(None, &va);
        assert_eq!(
            board,
            "* written by aispice for this run\nosdi '/tmp/aispice-run-x/comparator.osdi'\nset ngbehavior=ltpsa\n"
        );
        let pdk = PdkRun {
            osdi: vec![PathBuf::from("/pdk/psp103.osdi")],
            xyce_plugins: Vec::new(),
        };
        let ic = Ngspice::default().spiceinit_with(Some(&pdk), &va);
        assert!(
            ic.contains("osdi '/pdk/psp103.osdi'\nosdi '/tmp/aispice-run-x/comparator.osdi'\n")
        );
        assert!(!ic.contains("ngbehavior"));
        let deck = "t\nAcnt [clk] [q] null counter\n.tran 1u 1m\n.end\n";
        let cosim = vec![("counter".to_string(), "cosim_counter.so".to_string())];
        assert_eq!(
            with_cosim_models(deck, &cosim),
            "t\nAcnt [clk] [q] null counter\n.tran 1u 1m\n* co-simulation models added by aispice\n.model counter d_cosim simulation=\"./cosim_counter.so\"\n.end\n"
        );
        assert_eq!(with_cosim_models(deck, &[]), deck);
        assert!(!plain_path("/tmp/it's"));
        assert!(!plain_path("/tmp/a\\b"));
    }

    /// Unix only: a Windows temporary folder has backslashes, which a
    /// `.spiceinit` path may not, so no library loads there at all.
    #[cfg(unix)]
    #[test]
    fn a_library_that_does_not_match_its_hash_is_not_loaded() {
        let ws = Workspace::new().unwrap();
        let good = Arc::new(Library {
            kind: LibraryKind::Osdi,
            name: "m".into(),
            modules: Vec::new(),
            bytes: Arc::from(b"osdi bytes".to_vec()),
            sha256: sha256_hex(b"osdi bytes"),
            tool: String::new(),
        });
        let placed = place_libraries(&ws, std::slice::from_ref(&good)).unwrap();
        assert_eq!(std::fs::read(&placed[0].1).unwrap(), b"osdi bytes");
        let bad = Arc::new(Library {
            sha256: sha256_hex(b"other"),
            ..(*good).clone()
        });
        assert!(place_libraries(&ws, &[bad]).is_err());
    }
}
