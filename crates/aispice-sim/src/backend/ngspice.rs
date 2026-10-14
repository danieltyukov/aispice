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
//! it. No path a deck or a project names is ever loaded. A run that loads
//! any of them runs in a sandbox, since their code runs inside ngspice:
//! see [`super::native_code`]. Every other run goes in the same sandbox
//! under the denylist where one can start, and unconfined, with a warning
//! in its output, where none can.
//!
//! ngspice refuses to evaluate `.meas` in batch mode when it writes a raw
//! file, so a deck with measurements runs twice: once for the raw file and
//! once, without it, for the measurements. ngspice runs take milliseconds;
//! the job's timeout covers both.

use super::native_code;
use super::process::{self, ProcessSpec};
use super::{
    Detection, RAW_SIZE_LIMIT, SimError, SimId, SimJob, SimOutput, Simulator, Workspace, finish,
    read_output, which,
};
use crate::behavioral::{Library, LibraryKind, sha256_hex};
use crate::log::parse_ngspice_output;
use crate::pdk::PdkRun;
use crate::sandbox::{Plan, Profile, Sandbox};
use std::ffi::OsString;
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

/// The sandbox plan of a run in `ws`: the preloaded library placed there,
/// and reads of the job's files, its PDK's compiled models and ngspice's
/// install. A run that loads no compiled models of the project's gets the
/// denylist and its process margin.
async fn confine(
    job: &SimJob,
    exe: &Path,
    ws: &Workspace,
    cancel: &CancellationToken,
) -> Result<Plan, SimError> {
    let preload = native_code::preload(cancel).await?;
    native_code::place_preload(ws.path(), &preload)?;
    let mut reads = job.reads.clone();
    if let Some(pdk) = &job.pdk {
        reads.extend(pdk.osdi.iter().cloned());
    }
    let mut plan = native_code::plan(exe, ws.path(), &reads, Vec::new())?;
    if job.libraries.is_empty() {
        plan.profile = Profile::Denylist;
        plan.limits.nproc_headroom = Some(native_code::DENYLIST_NPROC_HEADROOM);
    }
    Ok(plan)
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
        // create_new, so two libraries that resolve to the same file name
        // (which the project config refuses up front) cannot silently
        // overwrite each other here.
        let path = ws.file(&lib.file_name());
        {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .and_then(|mut f| f.write_all(&lib.bytes))
                .map_err(|e| SimError::Io(format!("cannot write {}: {e}", lib.file_name())))?;
        }
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
        d.notes.push(match native_code::plain_sandbox().await {
            Ok(Ok(s)) => format!("runs in a sandbox: {}", s.describe()),
            Ok(Err(why)) => format!("runs without a sandbox ({why})"),
            Err(e) => format!("could not tell whether it runs in a sandbox: {e}"),
        });
        d
    }

    async fn run(&self, job: &SimJob, cancel: &CancellationToken) -> Result<SimOutput, SimError> {
        let exe = self
            .exe()
            .ok_or_else(|| SimError::NotFound("ngspice".into()))?;
        let stem = job.stem();
        let cosim: Vec<(String, String)> = job
            .libraries
            .iter()
            .filter(|l| l.kind == LibraryKind::Cosim)
            .map(|l| (l.name.clone(), l.file_name()))
            .collect();
        let deck = with_cosim_models(&job.netlist_text, &cosim);
        // Code the project compiled, and control commands the user allowed,
        // run only in the sandbox, with the preloaded library that keeps
        // ngspice's temporary file in the run folder. Compiled code gets
        // the native-code allowlist; control commands alone get the
        // denylist. A run with neither, such as a PDK run, gets the
        // denylist where this machine can give one, and otherwise runs
        // unconfined, as before, with a warning; once a sandbox is chosen,
        // a run that cannot be set up in it fails rather than run outside.
        let how = if !job.libraries.is_empty() || job.control {
            let purpose = if job.libraries.is_empty() {
                native_code::Purpose::ControlCommands
            } else {
                native_code::Purpose::CompiledModels
            };
            Confine::Sandboxed(match &job.sandbox {
                Some(s) => s.clone(),
                None => native_code::ready_for(purpose, cancel)
                    .await
                    .map_err(SimError::Config)?,
            })
        } else {
            let sandbox = match &job.sandbox {
                Some(s) => Ok(s.clone()),
                None => native_code::plain_sandbox().await?,
            };
            match sandbox {
                Ok(s) => Confine::Sandboxed(s),
                Err(why) => Confine::Unconfined(native_code::unconfined_warning(&why)),
            }
        };
        let started = std::time::Instant::now();
        let main = self
            .prepare(
                job,
                &exe,
                &format!("{stem}.cir"),
                &deck,
                Self::args(&stem),
                &how,
                cancel,
            )
            .await?;
        let out = process::run(&main.spec, job.timeout, cancel).await?;
        let mut log = out.combined();
        let mut report = parse_ngspice_output(&log);
        if has_meas(&deck) && report.errors.is_empty() {
            // ngspice will not evaluate .meas in batch mode when writing a
            // raw file, so the measurements come from a second run without
            // one, in a fresh folder of its own, within the same deadline.
            let left = job.timeout.saturating_sub(started.elapsed());
            let meas = self
                .prepare(
                    job,
                    &exe,
                    &format!("{stem}_meas.cir"),
                    &measurement_deck(&deck),
                    Self::meas_args(&stem),
                    &how,
                    cancel,
                )
                .await?;
            let m = process::run(&meas.spec, left, cancel).await?;
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
        let datasets = match read_output(&main.ws.file(&format!("{stem}.raw")), RAW_SIZE_LIMIT)? {
            Some(bytes) => crate::raw::read_raw(&bytes)?,
            None => Vec::new(),
        };
        let files = main.ws.keep(
            &job.run_dir,
            &[format!("{stem}.cir"), format!("{stem}.raw")],
        )?;
        let raw_path = files
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "raw"))
            .cloned();
        let mut warnings = report.warnings;
        if let Confine::Unconfined(w) = how {
            warnings.insert(0, w);
        }
        let result = SimOutput {
            datasets,
            log,
            measurements: report.measurements,
            errors: report.errors,
            warnings,
            duration: out.elapsed,
            raw_path,
            files,
        };
        finish(result, out.success(), "ngspice wrote no results")
    }
}

/// How a run's ngspice processes are confined.
enum Confine {
    /// In this sandbox, or not at all.
    Sandboxed(Sandbox),
    /// Unconfined, with this warning: the run loads none of the project's
    /// code, and this machine has no sandbox for it.
    Unconfined(String),
}

/// One ngspice process: its private folder and the command that runs it
/// there.
struct Process {
    ws: Workspace,
    spec: ProcessSpec,
}

impl Ngspice {
    /// A fresh private folder for one ngspice process, holding only what
    /// aispice writes there before it starts (the deck as `name`, the
    /// `.spiceinit`, the project's libraries and, in a sandbox, the
    /// preloaded library), and the command that runs it there with `args`.
    /// aispice writes nothing into the folder once ngspice has run in it.
    /// A run that cannot be set up in its sandbox fails: an integrity
    /// check, a cancellation or a timeout never sends it outside.
    #[allow(clippy::too_many_arguments)]
    async fn prepare(
        &self,
        job: &SimJob,
        exe: &Path,
        name: &str,
        deck: &str,
        args: Vec<String>,
        how: &Confine,
        cancel: &CancellationToken,
    ) -> Result<Process, SimError> {
        let ws = Workspace::new()?;
        let placed = place_libraries(&ws, &job.libraries)?;
        let osdi: Vec<PathBuf> = placed
            .iter()
            .filter(|(l, _)| l.kind == LibraryKind::Osdi)
            .map(|(_, p)| p.clone())
            .collect();
        ws.create(name, deck.as_bytes())?;
        ws.create(
            ".spiceinit",
            self.spiceinit_with(job.pdk.as_ref(), &osdi).as_bytes(),
        )?;
        let sandboxed = match how {
            Confine::Sandboxed(s) => Some((s.clone(), confine(job, exe, &ws, cancel).await?)),
            Confine::Unconfined(_) => None,
        };
        let spec = match sandboxed {
            Some((sandbox, mut plan)) => {
                plan.args = args.into_iter().map(OsString::from).collect();
                sandbox.command(&plan)?
            }
            None => {
                let mut spec = ProcessSpec::new(exe, ws.path())
                    .env("HOME", ws.path())
                    .env("USERPROFILE", ws.path());
                for a in args {
                    spec = spec.arg(a);
                }
                spec
            }
        };
        Ok(Process { ws, spec })
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

    /// Each ngspice process of a run starts in a fresh folder holding only
    /// what aispice wrote there before it: the measurement run does not
    /// reuse the folder the first run wrote in.
    #[tokio::test]
    async fn every_ngspice_process_starts_in_a_fresh_folder() {
        let job = SimJob::new("t\n.end\n", "runs");
        let exe = PathBuf::from("ngspice");
        let how = Confine::Unconfined(String::new());
        let ng = Ngspice::default();
        let cancel = CancellationToken::new();
        let deck = "t\n.meas op x find v(a)\n.end\n";
        let main = ng
            .prepare(&job, &exe, "d.cir", deck, Ngspice::args("d"), &how, &cancel)
            .await
            .unwrap();
        let meas = ng
            .prepare(
                &job,
                &exe,
                "d_meas.cir",
                &measurement_deck(deck),
                Ngspice::meas_args("d"),
                &how,
                &cancel,
            )
            .await
            .unwrap();
        assert_ne!(main.ws.path(), meas.ws.path());
        let names = |p: &Process| {
            let mut v: Vec<String> = std::fs::read_dir(p.ws.path())
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        };
        assert_eq!(names(&main), [".spiceinit", "d.cir"]);
        assert_eq!(names(&meas), [".spiceinit", "d_meas.cir"]);
        for p in [&main, &meas] {
            assert_eq!(p.spec.cwd, p.ws.path());
        }
    }

    /// Once a sandbox is chosen, a process that cannot be set up in it
    /// fails, and never runs outside instead: here ngspice cannot be
    /// resolved for the plan. Unconfined, the same process is set up.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_process_that_cannot_be_set_up_in_its_sandbox_fails() {
        if native_code::compiler().is_none() {
            eprintln!("skipped: no C compiler");
            return;
        }
        let job = SimJob::new("t\n.end\n", "runs");
        let missing = PathBuf::from("/nonexistent/aispice/bin/ngspice");
        let ng = Ngspice::default();
        let cancel = CancellationToken::new();
        let prepare = |how: Confine| {
            let (job, missing, ng, cancel) = (&job, &missing, &ng, &cancel);
            async move {
                ng.prepare(
                    job,
                    missing,
                    "d.cir",
                    "t\n.end\n",
                    Ngspice::args("d"),
                    &how,
                    cancel,
                )
                .await
                .map(|p| p.spec)
            }
        };
        let e = prepare(Confine::Sandboxed(Sandbox::landlock_for_tests(6)))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("cannot resolve"), "{e}");
        let spec = prepare(Confine::Unconfined(String::new())).await.unwrap();
        assert_eq!(spec.program, missing);
    }

    /// A run that loads none of the project's compiled models gets the
    /// denylist and its process margin, with the job's reads and its PDK's
    /// models; one that loads them keeps the native-code profile.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_run_without_compiled_models_gets_the_denylist() {
        if native_code::compiler().is_none() {
            eprintln!("skipped: no C compiler");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let [exe, inc, osdi] = ["ngspice", "r.inc", "psp103.osdi"].map(|f| {
            std::fs::write(root.join(f), "").unwrap();
            root.join(f)
        });
        let mut job = SimJob::new("t\n.end\n", root.join("runs"));
        job.reads = vec![inc.clone()];
        job.pdk = Some(PdkRun {
            osdi: vec![osdi.clone()],
            xyce_plugins: Vec::new(),
        });
        let cancel = CancellationToken::new();
        let ws = Workspace::new().unwrap();
        let plan = confine(&job, &exe, &ws, &cancel).await.unwrap();
        assert_eq!(plan.profile, Profile::Denylist);
        assert_eq!(
            plan.limits.nproc_headroom,
            Some(native_code::DENYLIST_NPROC_HEADROOM)
        );
        assert!(plan.read.contains(&inc) && plan.read.contains(&osdi));
        assert_eq!(plan.write, vec![ws.path().canonicalize().unwrap()]);
        assert!(ws.path().join(native_code::PRELOAD_FILE).is_file());

        job.libraries = vec![Arc::new(Library {
            kind: LibraryKind::Osdi,
            name: "m".into(),
            modules: Vec::new(),
            bytes: Arc::from(b"osdi bytes".to_vec()),
            sha256: sha256_hex(b"osdi bytes"),
            tool: String::new(),
        })];
        let ws = Workspace::new().unwrap();
        let plan = confine(&job, &exe, &ws, &cancel).await.unwrap();
        assert_eq!(plan.profile, Profile::NativeCode);
        assert_ne!(
            plan.limits.nproc_headroom,
            Some(native_code::DENYLIST_NPROC_HEADROOM)
        );
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
