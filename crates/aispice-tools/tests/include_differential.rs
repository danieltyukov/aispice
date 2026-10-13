//! The netlist policy against the simulators themselves. Each spelling of an
//! include line is tried in a file the deck includes, which the simulator
//! reads as written, and as a line of the deck itself, which goes through
//! the runner. Either the policy refuses it, or ngspice (in its native mode
//! and in the LTspice mode aispice runs it in) and Xyce never open a file
//! outside the project.
//!
//! A decoy library outside the project defines the parameter the circuit
//! prints with a sentinel value, reachable only through symbolic links and
//! spellings the policy has to see through. A run that saw it prints the
//! sentinel or names it in its log. Where `strace` is installed, every file
//! the simulators open is checked as well, except the runner's ngspice
//! where it runs in a sandbox.
//!
//! Unix only (symbolic links). Needs ngspice; the Xyce half needs Xyce
//! (`AISPICE_XYCE`, else `Xyce` on PATH) and is skipped without it.

#![cfg(unix)]

use aispice_core::netlist::{Policy, check_policy};
use aispice_sim::backend::which;
use aispice_tools::runner::RunError;
use aispice_tools::{Project, RunMods, Runner, RunnerConfig};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tokio_util::sync::CancellationToken;

/// The value only the decoy gives the probe parameter.
const SENTINEL: f64 = 271828.0;

/// The decoy. Whatever reads it either prints the sentinel or names it in
/// an error: its first line includes a missing file named after it, which
/// every simulator reports, even ngspice in LTspice mode, which then fails
/// on the section header it reads as another include.
const DECOY: &str = "* decoy outside the project\n.include missing-271828.lib\n.param probe=271828\n.lib sec\n.param probe=271828\n.endl sec\n";
const APPROVED: &str = "* approved\n.param probe=1\n";
const APPROVED_SECTIONS: &str = "* approved, with a section\n.lib sec\n.param probe=2\n.endl sec\n";

/// The circuit after the include: it prints the probe.
const CIRCUIT: &str = "V1 a 0 {probe}\nR1 a 0 1k\n.op\n.end\n";

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    proj: PathBuf,
    outside: PathBuf,
    strace: Option<PathBuf>,
}

/// `proj` with approved libraries (`ok.lib`, also under a name with a
/// space, and `oksec.lib` with a section), and links to the decoy in
/// `outside`: `link.lib`, `linkdir/` and `my`, which is what ngspice opens
/// for a `.lib "my lib.lib"` line.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let proj = root.join("proj");
    let outside = root.join("outside");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("decoy.lib"), DECOY).unwrap();
    std::fs::write(proj.join("ok.lib"), APPROVED).unwrap();
    std::fs::write(proj.join("oksec.lib"), APPROVED_SECTIONS).unwrap();
    std::fs::write(proj.join("my lib.lib"), APPROVED).unwrap();
    symlink(outside.join("decoy.lib"), proj.join("link.lib")).unwrap();
    symlink(&outside, proj.join("linkdir")).unwrap();
    symlink(outside.join("decoy.lib"), proj.join("my")).unwrap();
    // Only a strace that can trace here (ptrace may be locked down).
    let strace = which("strace").filter(|s| {
        Command::new(s)
            .args(["-f", "-qq", "-o", "/dev/null", "true"])
            .status()
            .is_ok_and(|st| st.success())
    });
    if strace.is_none() {
        eprintln!("note: no working strace; checking printed values and logs only");
    }
    Fixture {
        _dir: dir,
        root,
        proj,
        outside,
        strace,
    }
}

/// What a case expects of the policy.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    /// Refused.
    Refused,
    /// Accepted, and ngspice in native mode and Xyce read this probe value
    /// from the approved library.
    Reads(f64),
    /// Accepted; what the simulators make of it does not matter as long as
    /// they stay in the project.
    Accepted,
}

/// Each spelling, as the lines of a file, and what the policy should make
/// of it in a file the deck includes and as a line of the deck. `{p}` is
/// the project folder, `{o}` the folder outside it.
///
/// A deck line differs from a line in an included file in two ways. aispice
/// reads the deck itself and writes it out again, without trailing comments
/// and with continuation lines joined, so a spelling that is ambiguous in a
/// file can be plain in the deck. And ngspice resolves a relative `.lib`
/// path in an included file against the deck's folder rather than the
/// file's, so such a line reads nothing (the policy's resolution is the
/// stricter one).
fn cases(fx: &Fixture) -> Vec<(&'static str, String, Expect, Expect)> {
    use Expect::*;
    let raw: Vec<(&str, &str, Expect, Expect)> = vec![
        ("plain", ".include ok.lib", Reads(1.0), Reads(1.0)),
        ("quoted", ".include \"ok.lib\"", Reads(1.0), Reads(1.0)),
        ("keyword case", ".INC ok.lib", Reads(1.0), Reads(1.0)),
        ("tab", ".include\tok.lib", Reads(1.0), Reads(1.0)),
        (
            "trailing blanks",
            ".include ok.lib \t ",
            Reads(1.0),
            Reads(1.0),
        ),
        ("dot slash", ".include ./ok.lib", Reads(1.0), Reads(1.0)),
        ("absolute", ".include {p}/ok.lib", Reads(1.0), Reads(1.0)),
        (
            "space in a quoted include",
            ".include \"my lib.lib\"",
            Reads(1.0),
            Reads(1.0),
        ),
        ("section", ".lib oksec.lib sec", Accepted, Reads(2.0)),
        (
            "section, absolute",
            ".lib {p}/oksec.lib sec",
            Reads(2.0),
            Reads(2.0),
        ),
        (
            "section, quoted and upper case",
            ".LIB \"{p}/oksec.lib\" SEC",
            Reads(2.0),
            Reads(2.0),
        ),
        ("section header only", ".lib sec", Accepted, Accepted),
        ("symlink", ".include link.lib", Refused, Refused),
        (
            "symlinked folder",
            ".include linkdir/decoy.lib",
            Refused,
            Refused,
        ),
        (
            "parent folder",
            ".include ../outside/decoy.lib",
            Refused,
            Refused,
        ),
        (
            "outside, absolute",
            ".include {o}/decoy.lib",
            Refused,
            Refused,
        ),
        ("one-argument lib", ".lib link.lib", Refused, Refused),
        (
            "section of a symlink",
            ".lib link.lib sec",
            Refused,
            Refused,
        ),
        ("single quotes", ".include 'link.lib'", Refused, Refused),
        (
            "single quotes, approved",
            ".include 'ok.lib'",
            Refused,
            Refused,
        ),
        ("semicolon glued", ".include link.lib;x", Refused, Refused),
        (
            "semicolon comment",
            ".include ok.lib ; link.lib",
            Refused,
            Reads(1.0),
        ),
        ("dollar glued", ".include link.lib$x", Refused, Refused),
        (
            "dollar comment",
            ".include ok.lib $ link.lib",
            Refused,
            Reads(1.0),
        ),
        ("double slash", ".include link.lib//x", Refused, Refused),
        (
            "double slash, absolute",
            ".include {p}/link.lib//x",
            Refused,
            Refused,
        ),
        ("two files", ".include ok.lib link.lib", Refused, Refused),
        (
            "text after quotes",
            ".include \"ok.lib\"link.lib",
            Refused,
            Refused,
        ),
        ("unclosed quote", ".include \"link.lib", Refused, Refused),
        (
            "space in a lib path",
            ".lib \"my lib.lib\" sec",
            Refused,
            Refused,
        ),
        ("tilde", ".include ~/decoy.lib", Refused, Refused),
        (
            "environment variable",
            ".include $HOME/decoy.lib",
            Refused,
            Refused,
        ),
        ("braces", ".include {link.lib}", Refused, Refused),
        ("brackets", ".include (link.lib)", Refused, Refused),
        ("comma", ".include link.lib,x", Refused, Refused),
        ("equals", ".include=link.lib", Refused, Refused),
        (
            "extra section word",
            ".lib oksec.lib sec link.lib",
            Refused,
            Refused,
        ),
        (
            "backslashes",
            ".include linkdir\\decoy.lib",
            Refused,
            Refused,
        ),
        ("no-break space", ".include\u{a0}link.lib", Refused, Refused),
        ("indented", "  .include link.lib", Refused, Refused),
        (
            "continued include",
            ".include\n+ link.lib",
            Refused,
            Refused,
        ),
        (
            "continued lib section",
            ".lib {p}/oksec.lib\n+ sec",
            Refused,
            Reads(2.0),
        ),
        (
            "continued past an indented line",
            ".lib\n  R9 x 0 1\n+ link.lib sec",
            Refused,
            Refused,
        ),
        (
            "continued past a semicolon line",
            ".lib\n; x\n+ link.lib sec",
            Refused,
            Refused,
        ),
        (
            "continued past a dollar line",
            ".lib\n$ x\n+ link.lib sec",
            Refused,
            Refused,
        ),
        (
            "continued past a comment",
            ".lib\n* x\n+ link.lib sec",
            Refused,
            Refused,
        ),
        (
            "double backslash join",
            "R9 x 0 1 \\\\\nlink.lib sec",
            Refused,
            Refused,
        ),
        (
            "lone carriage return",
            ".include ok.lib\r.include link.lib",
            Refused,
            Refused,
        ),
        (
            "line separator",
            ".include ok.lib\u{2028}.include link.lib",
            Refused,
            Refused,
        ),
    ];
    let p = fx.proj.display().to_string();
    let o = fx.outside.display().to_string();
    raw.into_iter()
        .map(|(name, text, nested, deck)| {
            (
                name,
                text.replace("{p}", &p).replace("{o}", &o),
                nested,
                deck,
            )
        })
        .collect()
}

fn policy(fx: &Fixture) -> Policy {
    Policy {
        base_dir: fx.proj.clone(),
        allowed_dirs: vec![fx.proj.clone()],
        ..Policy::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Direct {
    NgspiceNative,
    NgspiceLtspiceMode,
    Xyce,
}

/// What a run let on about the files it read.
#[derive(Debug, Default)]
struct Seen {
    probe: Option<f64>,
    /// The sentinel in the output or the log.
    sentinel: bool,
    /// Files outside the project it opened, by `strace`.
    opened_outside: Vec<String>,
    /// Every file it opened, by `strace`.
    opened: Vec<String>,
}

impl Seen {
    fn saw_decoy(&self) -> bool {
        self.sentinel || self.probe == Some(SENTINEL) || !self.opened_outside.is_empty()
    }
}

/// Files a traced process opened: the paths `strace -y` gives the file
/// descriptors that open calls returned, from every `<prefix>.<pid>` file.
fn opened_files(prefix: &Path) -> Vec<String> {
    let dir = prefix.parent().unwrap();
    let stem = prefix.file_name().unwrap().to_string_lossy().into_owned();
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&format!("{stem}.")) {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        for line in text.lines() {
            if let Some((_, ret)) = line.rsplit_once(" = ")
                && let Some(path) = ret.split_once('<').and_then(|(_, r)| r.split_once('>'))
            {
                out.push(path.0.to_string());
            }
        }
        std::fs::remove_file(entry.path()).unwrap();
    }
    out
}

fn sentinel_in(text: &str) -> bool {
    text.contains("271828")
}

fn probe_of(raw: &Path) -> Option<f64> {
    let sets = aispice_sim::raw::read_raw_file(raw).ok()?;
    sets.iter()
        .find_map(|d| d.vector("V(a)").or_else(|| d.vector("v(a)")))
        .and_then(|v| v.data.real().first().copied())
}

fn seen(fx: &Fixture, mut s: Seen, trace: &Path) -> Seen {
    if fx.strace.is_some() {
        s.opened = opened_files(trace);
        let outside = fx.outside.display().to_string();
        s.opened_outside = s
            .opened
            .iter()
            .filter(|p| p.starts_with(&outside))
            .cloned()
            .collect();
    }
    s
}

/// Run a deck as written, outside aispice, the way the backends run it: in
/// a private folder that is also HOME, with aispice's `.spiceinit`.
fn run_direct(fx: &Fixture, sim: Direct, exe: &Path, deck: &str, n: usize) -> Seen {
    let run = fx.root.join(format!("direct-{n}-{sim:?}"));
    std::fs::create_dir_all(&run).unwrap();
    std::fs::write(run.join("deck.cir"), deck).unwrap();
    let args: Vec<&str> = match sim {
        Direct::NgspiceNative => {
            std::fs::write(run.join(".spiceinit"), "* native\n").unwrap();
            vec!["-b", "-r", "deck.raw", "deck.cir"]
        }
        Direct::NgspiceLtspiceMode => {
            std::fs::write(run.join(".spiceinit"), "set ngbehavior=ltpsa\n").unwrap();
            vec!["-b", "-r", "deck.raw", "deck.cir"]
        }
        Direct::Xyce => vec!["-r", "deck.raw", "deck.cir"],
    };
    let trace = fx.root.join(format!("trace-{n}"));
    let mut cmd = match &fx.strace {
        Some(strace) => {
            let mut c = Command::new(strace);
            c.args(["-f", "-ff", "-qq", "-y", "-e", "trace=%file", "-o"])
                .arg(&trace)
                .arg(exe);
            c
        }
        None => Command::new(exe),
    };
    let out = cmd
        .args(&args)
        .current_dir(&run)
        .env("HOME", &run)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let s = Seen {
        probe: probe_of(&run.join("deck.raw")),
        sentinel: sentinel_in(&log),
        ..Seen::default()
    };
    seen(fx, s, &trace)
}

/// A shell script that runs `exe` under strace, writing to `trace`, for
/// the runner to call in its place.
fn traced(fx: &Fixture, name: &str, exe: &Path, trace: &Path) -> PathBuf {
    let dir = fx.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join(name);
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nexec '{}' -f -ff -qq -y -e trace=%file -o '{}' '{}' \"$@\"\n",
            fx.strace.as_ref().unwrap().display(),
            trace.display(),
            exe.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn deck_including(file: &Path) -> String {
    format!("* differential\n.include \"{}\"\n{CIRCUIT}", file.display())
}

#[test]
fn include_spellings_never_reach_a_file_outside_the_project() {
    let Some(ngspice) = which("ngspice") else {
        eprintln!("skipped: ngspice is not installed");
        return;
    };
    let xyce = std::env::var_os("AISPICE_XYCE")
        .map(PathBuf::from)
        .or_else(|| which("Xyce"));
    if xyce.is_none() {
        eprintln!("note: Xyce is not installed; its half is skipped");
    }
    let fx = fixture();
    let mut sims = vec![
        (Direct::NgspiceNative, ngspice.clone()),
        (Direct::NgspiceLtspiceMode, ngspice.clone()),
    ];
    if let Some(x) = &xyce {
        sims.push((Direct::Xyce, x.clone()));
    }

    // The detector works: a deck that names the decoy outright, run
    // without the policy, is caught on every simulator.
    let decoy = deck_including(&fx.outside.join("decoy.lib"));
    for (i, (sim, exe)) in sims.iter().enumerate() {
        let s = run_direct(&fx, *sim, exe, &decoy, 1000 + i);
        assert!(s.sentinel, "{sim:?}: {s:?}");
        if fx.strace.is_some() {
            assert!(!s.opened_outside.is_empty(), "{sim:?}: {s:?}");
        }
    }

    // In a file the deck includes, which no simulator sees rewritten.
    let mut failures = Vec::new();
    // Refused spellings that do reach the decoy when run unchecked.
    let mut exploits: Vec<(&str, Direct)> = Vec::new();
    for (n, (name, text, expect, _)) in cases(&fx).into_iter().enumerate() {
        let wrapper = fx.proj.join(format!("wrapper-{n}.lib"));
        std::fs::write(&wrapper, format!("* wrapper\n{text}\n")).unwrap();
        let deck = deck_including(&wrapper);
        let violations = check_policy(&deck, &policy(&fx));
        match (expect, violations.is_empty()) {
            (Expect::Refused, true) => failures.push(format!("{name}: accepted, expected refused")),
            (Expect::Refused, false) => {
                for (sim, exe) in &sims {
                    if run_direct(&fx, *sim, exe, &deck, n).saw_decoy() {
                        exploits.push((name, *sim));
                    }
                }
                continue;
            }
            (_, false) => {
                failures.push(format!("{name}: refused: {violations:?}"));
                continue;
            }
            _ => {}
        }
        for (sim, exe) in &sims {
            let s = run_direct(&fx, *sim, exe, &deck, n);
            if s.saw_decoy() {
                failures.push(format!(
                    "{name}, included, {sim:?}: reached the decoy: {s:?}"
                ));
            }
            if let Expect::Reads(v) = expect
                && *sim != Direct::NgspiceLtspiceMode
                && s.probe != Some(v)
            {
                failures.push(format!(
                    "{name}, included, {sim:?}: probe {:?}, expected {v}",
                    s.probe
                ));
            }
        }
    }

    // The refusals are not hypothetical: unchecked, these spellings take
    // the simulator to the decoy (ngspice 42 and Xyce 7.10).
    let mut expected = vec![
        ("symlink", Direct::NgspiceNative),
        ("single quotes", Direct::NgspiceNative),
        ("semicolon glued", Direct::NgspiceNative),
        ("double slash", Direct::NgspiceNative),
        ("indented", Direct::NgspiceNative),
        ("one-argument lib", Direct::NgspiceLtspiceMode),
    ];
    if xyce.is_some() {
        expected.extend([
            ("braces", Direct::Xyce),
            ("comma", Direct::Xyce),
            ("continued include", Direct::Xyce),
            ("continued past an indented line", Direct::Xyce),
            ("continued past a semicolon line", Direct::Xyce),
            ("continued past a comment", Direct::Xyce),
            ("lone carriage return", Direct::Xyce),
        ]);
    }
    for (name, sim) in expected {
        if !exploits.contains(&(name, sim)) {
            failures.push(format!(
                "{name}, unchecked, {sim:?}: expected to reach the decoy, which shows the detector or the case is broken"
            ));
        }
    }

    // As a line of the deck, through the runner: the policy, the rewrite of
    // the deck's include lines and the simulator as aispice runs it.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ng_trace = fx.root.join("runner-ngspice");
    let xy_trace = fx.root.join("runner-xyce");
    // Where a sandbox can start, the runner's ngspice runs in it, and the
    // strace wrapper cannot run there: the denylist refuses ptrace, and the
    // trace goes outside the run folder. The sandbox then keeps ngspice
    // from opening anything the deck was not granted, and its values and
    // logs are checked as everywhere.
    let confined = rt
        .block_on(aispice_sim::backend::native_code::plain_sandbox())
        .is_ok();
    let ng_traced = fx.strace.is_some() && !confined;
    let mut runner_sims = vec![("ngspice", ng_trace.clone(), ng_traced)];
    let xyce_exe = match (&xyce, &fx.strace) {
        (Some(x), Some(_)) => Some(traced(&fx, "Xyce", x, &xy_trace)),
        (x, _) => x.clone(),
    };
    if xyce_exe.is_some() {
        runner_sims.push(("xyce", xy_trace.clone(), fx.strace.is_some()));
    }
    let ngspice_exe = if ng_traced {
        traced(&fx, "ngspice", &ngspice, &ng_trace)
    } else {
        ngspice.clone()
    };
    let runner = Runner::new(RunnerConfig {
        ngspice_exe: Some(ngspice_exe),
        xyce_exe,
        embedded_models_only: true,
        ..RunnerConfig::default()
    });
    let project = Project::open(&fx.proj, Default::default()).unwrap();
    let mut top: Vec<(&str, String, Expect)> = cases(&fx)
        .into_iter()
        .map(|(name, text, _, deck)| (name, text, deck))
        .collect();
    // ngspice includes the file a title line names; Xyce reads it as text.
    top.push(("title line", String::new(), Expect::Refused));
    top.push(("approved title line", String::new(), Expect::Accepted));
    for (n, (name, text, expect)) in top.into_iter().enumerate() {
        let deck = match name {
            "title line" => format!(".include link.lib\n{CIRCUIT}"),
            "approved title line" => format!(".include ok.lib\n{CIRCUIT}"),
            _ => format!("* differential\n{text}\n{CIRCUIT}"),
        };
        let circuit = format!("case-{n}.cir");
        std::fs::write(fx.proj.join(&circuit), &deck).unwrap();
        for (sim, trace, is_traced) in &runner_sims {
            let result = rt.block_on(runner.run_circuit(
                &project,
                &circuit,
                Some(sim),
                &RunMods::default(),
                &CancellationToken::new(),
            ));
            let s = match &result {
                Err(RunError::Policy(_)) if expect == Expect::Refused => {
                    if *is_traced {
                        let _ = opened_files(trace);
                    }
                    continue;
                }
                Err(RunError::Policy(why)) => {
                    failures.push(format!("{name}, deck, {sim}: refused: {why}"));
                    continue;
                }
                Ok(run) => Seen {
                    probe: run
                        .output
                        .datasets
                        .iter()
                        .find_map(|d| d.vector("V(a)"))
                        .and_then(|v| v.data.real().first().copied()),
                    sentinel: sentinel_in(&run.output.log) || sentinel_in(&run.deck),
                    ..Seen::default()
                },
                Err(e) => Seen {
                    sentinel: sentinel_in(&e.to_string()),
                    ..Seen::default()
                },
            };
            let s = if *is_traced { seen(&fx, s, trace) } else { s };
            if expect == Expect::Refused {
                failures.push(format!(
                    "{name}, deck, {sim}: accepted, expected refused: {result:?}"
                ));
            }
            if s.saw_decoy() {
                failures.push(format!("{name}, deck, {sim}: reached the decoy: {s:?}"));
            }
            if let Expect::Reads(v) = expect
                && s.probe != Some(v)
            {
                failures.push(format!(
                    "{name}, deck, {sim}: probe {:?}, expected {v}: {:?}",
                    s.probe,
                    result.as_ref().err()
                ));
            }
            if *is_traced && result.is_ok() && !s.opened.iter().any(|p| p.ends_with(".cir")) {
                failures.push(format!(
                    "{name}, deck, {sim}: the trace saw no deck opened: {s:?}"
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
