//! ngspice running code a user compiled, in every sandbox this machine has.
//!
//! A probe library, built here from C and handed to the backend as if
//! aispice had compiled it from a project's Verilog-A, runs as soon as
//! ngspice loads it. It tries what the sandbox must stop (a socket of every
//! family, a signal to a sibling of ngspice, to aispice and to everyone, a
//! file outside the run folder, in a folder it may only read and in `/tmp`,
//! reading a file it was not given) and what it must allow (a file in the
//! run folder, a signal to itself, `tmpfile()`, which must land in the run
//! folder), and prints what happened and its whole environment. The same
//! probe in an unconfined ngspice reaches everything, so a probe that
//! silently did nothing cannot pass. Skips without ngspice, a C compiler or
//! a sandbox, unless `AISPICE_REQUIRE_SANDBOXES` asks for one.

use super::*;
use crate::backend::ngspice::with_cosim_models;
use crate::backend::{Ngspice, SimError, SimJob, Simulator, which};
use crate::behavioral::{Library, LibraryKind, Source, verilog};
use crate::sandbox::every_available;
use std::process::{Child, Command};

const PROBE: &str = r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

extern char **environ;

static void report(const char *what, long r) {
    fprintf(stderr, "PROBE %s %s %d\n", what, r < 0 ? "refused" : "allowed", r < 0 ? errno : 0);
}

static void create(const char *what, const char *path) {
    int fd = open(path, O_CREAT | O_WRONLY | O_EXCL, 0600);
    report(what, fd);
    if (fd >= 0) close(fd);
}

__attribute__((constructor)) static void probe(void) {
    fprintf(stderr, "PROBE start\n");
    for (char **e = environ; *e; e++)
        fprintf(stderr, "PROBE env %.*s\n", (int) strcspn(*e, "="), *e);
    create("write-inside", "inside.txt");
    create("write-outside", OUTSIDE);
    create("write-readable", READABLE "/planted.txt");
    create("write-tmp", TMPTAG);
    int fd = open("/tmp", O_RDWR | O_TMPFILE, 0600);
    report("tmp-unnamed", fd);
    if (fd >= 0) close(fd);
    fd = open(SECRET, O_RDONLY);
    report("read-outside", fd);
    if (fd >= 0) close(fd);
    FILE *t = tmpfile();
    report("tmpfile", t ? 0 : -1);
    if (t) {
        char link[64], target[4096], cwd[4096];
        snprintf(link, sizeof link, "/proc/self/fd/%d", fileno(t));
        ssize_t n = readlink(link, target, sizeof target - 1);
        if (n >= 0 && getcwd(cwd, sizeof cwd)) {
            target[n] = '\0';
            fprintf(stderr, "PROBE tmpfile-at %s\n", target);
            size_t c = strlen(cwd);
            int inside = strncmp(target, cwd, c) == 0 && target[c] == '/';
            report("tmpfile-in-run", inside ? 0 : -1);
        }
        fclose(t);
    }
    int types[] = {SOCK_STREAM, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET};
    for (int family = 0; family < 64; family++) {
        for (int i = 0; i < 4; i++) {
            int s = socket(family, types[i], 0);
            if (s >= 0) {
                fprintf(stderr, "PROBE socket-%d-%d allowed 0\n", family, types[i]);
                close(s);
            }
        }
    }
    report("kill-sibling", kill(SIBLING, SIGNAL));
    report("tkill-sibling", syscall(SYS_tkill, SIBLING, SIGNAL));
    report("tgkill-sibling", syscall(SYS_tgkill, SIBLING, SIBLING, SIGNAL));
    siginfo_t si;
    memset(&si, 0, sizeof si);
    si.si_code = SI_QUEUE;
    si.si_pid = getpid();
    si.si_uid = getuid();
    report("sigqueue-sibling", syscall(SYS_rt_sigqueueinfo, SIBLING, SIGNAL, &si));
    long pidfd = syscall(SYS_pidfd_open, SIBLING, 0);
    if (pidfd >= 0)
        report("pidfd-signal-sibling", syscall(SYS_pidfd_send_signal, pidfd, SIGNAL, NULL, 0));
    report("kill-aispice", kill(AISPICE, 0));
    report("kill-everyone", kill(-1, 0));
    report("kill-self", kill(getpid(), 0));
    fprintf(stderr, "PROBE end\n");
}
"#;

/// The attempts that must fail in every sandbox. `kill(-1)` returns 0
/// whenever it had a target, even when every target refused, so Landlock's
/// signal scope alone would let it read as allowed while it reaches
/// nothing; seccomp's signal program refuses it outright. Writing outside
/// the run folder and in `/tmp` is refused under Landlock; bubblewrap lets
/// both land in its own `/tmp`, which nobody sees, so for both the test
/// looks for the files on the real filesystem.
const MUST_FAIL: &[&str] = &[
    "write-readable",
    "read-outside",
    "kill-sibling",
    "tkill-sibling",
    "tgkill-sibling",
    "sigqueue-sibling",
    "pidfd-signal-sibling",
    "kill-aispice",
    "kill-everyone",
];

/// What the probe sends its sibling in a sandbox: a signal that would end
/// it.
const SIGTERM: i32 = 15;

const DECK: &str = "* probe\nV1 a 0 1\nR1 a 0 1k\n.op\n.end\n";

/// Where one probe reaches: files and processes outside the run.
struct Targets {
    outside: PathBuf,
    readable: PathBuf,
    secret: PathBuf,
    tmp_tag: PathBuf,
}

impl Targets {
    fn new(dir: &Path, n: usize) -> Self {
        let readable = dir.join("readable");
        std::fs::create_dir_all(&readable).unwrap();
        let secret = dir.join("secret.txt");
        std::fs::write(&secret, "not for the model").unwrap();
        Self {
            outside: dir.join("escaped.txt"),
            readable,
            secret,
            tmp_tag: PathBuf::from(format!("/tmp/aispice-probe-{}-{n}", std::process::id())),
        }
    }
}

impl Drop for Targets {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.tmp_tag);
    }
}

/// The probe as a library aispice "built", reaching for `t`, `sibling`
/// with `signal`, and this test process.
fn probe(cc: &Path, dir: &Path, t: &Targets, sibling: u32, signal: i32) -> Library {
    let src = dir.join("probe.c");
    let so = dir.join(format!("probe-{signal}.so"));
    std::fs::write(&src, PROBE).unwrap();
    let define = |k: &str, v: &dyn std::fmt::Display| format!("-D{k}={v}");
    let quoted = |k: &str, p: &Path| format!("-D{k}=\"{}\"", p.display());
    let status = Command::new(cc)
        .args(["-shared", "-fPIC", "-O0", "-o"])
        .arg(&so)
        .arg(quoted("OUTSIDE", &t.outside))
        .arg(quoted("READABLE", &t.readable))
        .arg(quoted("SECRET", &t.secret))
        .arg(quoted("TMPTAG", &t.tmp_tag))
        .arg(define("SIBLING", &sibling))
        .arg(define("SIGNAL", &signal))
        .arg(define("AISPICE", &std::process::id()))
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success(), "the probe did not compile");
    let bytes = std::fs::read(&so).unwrap();
    Library {
        kind: LibraryKind::Osdi,
        name: "probe".into(),
        modules: Vec::new(),
        sha256: sha256_hex(&bytes),
        bytes: bytes.into(),
        tool: "cc".into(),
    }
}

/// The probe's report: each attempt and whether it was allowed.
fn attempts(log: &str) -> Vec<(String, bool)> {
    log.lines()
        .filter_map(|l| l.strip_prefix("PROBE "))
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            let what = w.next()?;
            let verdict = w.next()?;
            Some((what.to_string(), verdict == "allowed"))
        })
        .collect()
}

fn allowed(log: &str, what: &str) -> Option<bool> {
    attempts(log)
        .into_iter()
        .find(|(w, _)| w == what)
        .map(|(_, a)| a)
}

/// The names of the variables the probe saw, sorted.
fn environment_seen(log: &str) -> Vec<String> {
    let mut v: Vec<String> = log
        .lines()
        .filter_map(|l| l.strip_prefix("PROBE env "))
        .map(str::to_string)
        .collect();
    v.sort();
    v
}

/// ngspice's output with `lib` loaded through the backend in `sandbox`,
/// reading `reads`. A run without results stopped before the deck ran,
/// and shows nothing about what a working simulation can reach.
async fn sandboxed(
    sandbox: &Sandbox,
    lib: &Library,
    reads: Vec<PathBuf>,
    run_dir: &Path,
) -> String {
    let mut job = SimJob::new(DECK, run_dir);
    job.libraries = vec![Arc::new(lib.clone())];
    job.reads = reads;
    job.sandbox = Some(sandbox.clone());
    let what = sandbox.describe();
    match Ngspice::default()
        .run(&job, &CancellationToken::new())
        .await
    {
        Ok(out) if !out.datasets.is_empty() => out.log,
        Ok(out) => panic!("{what}: ngspice simulated nothing:\n{}", out.log),
        Err(SimError::Failed { log, .. }) => panic!("{what}: ngspice failed:\n{log}"),
        Err(e) => panic!("{what}: the sandboxed run did not start: {e}"),
    }
}

/// ngspice's output with the probe loaded and nothing confining it.
fn unconfined(exe: &Path, lib: &Library) -> String {
    let dir = tempfile::tempdir().unwrap();
    let so = dir.path().join(lib.file_name());
    std::fs::write(&so, &lib.bytes).unwrap();
    std::fs::write(
        dir.path().join(".spiceinit"),
        format!("osdi '{}'\n", so.display()),
    )
    .unwrap();
    std::fs::write(dir.path().join("probe.cir"), DECK).unwrap();
    let out = Command::new(exe)
        .args(["-b", "probe.cir"])
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .output()
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A process beside ngspice, both children of this test.
struct Sibling(Child);

impl Sibling {
    fn start() -> Self {
        Self(Command::new("sleep").arg("120").spawn().unwrap())
    }
    fn pid(&self) -> u32 {
        self.0.id()
    }
    fn alive(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }
}

impl Drop for Sibling {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// ngspice, a C compiler and every sandbox here, or why the test skips.
async fn setup() -> Option<(PathBuf, PathBuf, Vec<Sandbox>)> {
    let (Some(exe), Some(cc)) = (which("ngspice"), compiler()) else {
        eprintln!("skipped: needs ngspice and a C compiler");
        return None;
    };
    let sandboxes = every_available().await;
    if sandboxes.is_empty() {
        eprintln!("skipped: no sandbox here");
        return None;
    }
    Some((exe, cc, sandboxes))
}

#[tokio::test(flavor = "multi_thread")]
async fn compiled_code_in_a_sandboxed_ngspice_reaches_nothing_outside_its_run() {
    let Some((_, cc, sandboxes)) = setup().await else {
        return;
    };
    for (n, sandbox) in sandboxes.iter().enumerate() {
        let what = sandbox.describe();
        let dir = tempfile::tempdir().unwrap();
        let t = Targets::new(dir.path(), n);
        let mut sibling = Sibling::start();
        let lib = probe(&cc, dir.path(), &t, sibling.pid(), SIGTERM);
        let log = sandboxed(
            sandbox,
            &lib,
            vec![t.readable.clone()],
            &dir.path().join("runs"),
        )
        .await;
        assert!(
            log.contains("PROBE start") && log.contains("PROBE end"),
            "{what}: the probe did not run to the end:\n{log}"
        );
        for what_ok in ["write-inside", "kill-self", "tmpfile", "tmpfile-in-run"] {
            assert_eq!(
                allowed(&log, what_ok),
                Some(true),
                "{what}: {what_ok}:\n{log}"
            );
        }
        for what_not in MUST_FAIL {
            assert_ne!(
                allowed(&log, what_not),
                Some(true),
                "{what}: {what_not}:\n{log}"
            );
        }
        if !sandbox.is_bwrap() {
            for what_not in ["write-outside", "write-tmp", "tmp-unnamed"] {
                assert_eq!(
                    allowed(&log, what_not),
                    Some(false),
                    "{what}: {what_not}:\n{log}"
                );
            }
        }
        assert!(!t.tmp_tag.exists(), "{what}: a file appeared in /tmp");
        let sockets: Vec<String> = attempts(&log)
            .into_iter()
            .filter(|(w, _)| w.starts_with("socket-"))
            .map(|(w, _)| w)
            .collect();
        assert!(sockets.is_empty(), "{what}: sockets made: {sockets:?}");
        assert_eq!(
            environment_seen(&log),
            ["HOME", "LC_ALL", "LD_PRELOAD", "PATH", "TMPDIR"],
            "{what}:\n{log}"
        );
        assert!(!t.outside.exists(), "{what}: a file was written outside");
        assert!(
            !t.readable.join("planted.txt").exists(),
            "{what}: a file was written in a folder it may only read"
        );
        assert!(sibling.alive(), "{what}: the sibling was killed");
    }
}

/// The same probe, unconfined, does what the sandboxed runs refuse: the
/// test above would see an escape.
#[tokio::test(flavor = "multi_thread")]
async fn the_probe_reaches_all_of_it_when_ngspice_runs_unconfined() {
    let Some((exe, cc, _)) = setup().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let t = Targets::new(dir.path(), 99);
    let mut sibling = Sibling::start();
    // Signal 0 only checks that a signal could be sent.
    let lib = probe(&cc, dir.path(), &t, sibling.pid(), 0);
    let log = unconfined(&exe, &lib);
    for what in [
        "write-inside",
        "write-outside",
        "write-readable",
        "write-tmp",
        "read-outside",
        "kill-sibling",
        "kill-aispice",
        "kill-everyone",
        "kill-self",
    ] {
        assert_eq!(allowed(&log, what), Some(true), "{what}:\n{log}");
    }
    // glibc's own tmpfile() is in /tmp, not the folder ngspice runs in.
    assert_eq!(allowed(&log, "tmpfile-in-run"), Some(false), "{log}");
    assert!(
        attempts(&log)
            .iter()
            .any(|(w, a)| w.starts_with("socket-") && *a),
        "no socket was made:\n{log}"
    );
    assert!(environment_seen(&log).len() > 5, "{log}");
    assert!(t.outside.exists() && t.tmp_tag.exists());
    assert!(sibling.alive());
}

/// A ruleset below ABI 4 is never written: the run is refused before
/// ngspice starts.
#[tokio::test(flavor = "multi_thread")]
async fn below_landlock_4_compiled_code_is_not_loaded() {
    let Some((_, cc, _)) = setup().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let t = Targets::new(dir.path(), 98);
    let lib = probe(&cc, dir.path(), &t, std::process::id(), 0);
    let mut job = SimJob::new(DECK, dir.path().join("runs"));
    job.libraries = vec![Arc::new(lib)];
    job.sandbox = Some(Sandbox::landlock_for_tests(3));
    let e = Ngspice::default()
        .run(&job, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("ABI 3"), "{e}");
    assert!(!t.outside.exists());
}

/// The sentinel: Verilog the source check refuses (`$fopen` of a path
/// outside the run), built around the check as if the check were wrong.
/// Unconfined, ngspice running it writes the file; in every sandbox here
/// it does not, while the simulation itself runs.
#[tokio::test(flavor = "multi_thread")]
async fn a_model_the_source_check_refuses_still_cannot_write_outside_its_run() {
    let Some((ngspice, _, sandboxes)) = setup().await else {
        return;
    };
    let (Some(verilator), Some(cxx)) = (which("verilator"), verilog::cxx()) else {
        eprintln!("skipped: needs Verilator and a C++ compiler");
        return;
    };
    let Some(shim) = verilog::shim_dir(&ngspice) else {
        eprintln!("skipped: ngspice has no d_cosim sources");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("escaped.txt");
    let text = format!(
        "module escape(input clk, output reg q);\n  integer f;\n  initial begin\n    f = $fopen(\"{}\", \"w\");\n    $fdisplay(f, \"escaped\");\n    $fclose(f);\n    q = 1'b0;\n  end\nendmodule\n",
        outside.display()
    );
    let sources = [Source {
        name: "escape.v".into(),
        text,
    }];
    let refused = verilog::check(&sources).unwrap_err().to_string();
    assert!(refused.contains("$fopen"), "{refused}");
    let lib = verilog::build_checked(
        &verilator,
        "verilator (test)",
        &cxx,
        &shim,
        "escape",
        "escape",
        &sources,
        &["escape".to_string()],
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let deck = "* sentinel\nVclk clk_a 0 PULSE(0 1 0 1n 1n 5n 10n)\nAclk [clk_a] [clk] tobit\n.model tobit adc_bridge(in_low=0.3 in_high=0.7)\nAesc [clk] [q] null escape\n.tran 1n 50n\n.end\n";

    // Unconfined, the model writes outside.
    let plain = dir.path().join("plain");
    std::fs::create_dir(&plain).unwrap();
    std::fs::write(plain.join(lib.file_name()), &lib.bytes).unwrap();
    let cosim = [("escape".to_string(), lib.file_name())];
    std::fs::write(plain.join("deck.cir"), with_cosim_models(deck, &cosim)).unwrap();
    let out = Command::new(&ngspice)
        .args(["-b", "-r", "deck.raw", "deck.cir"])
        .current_dir(&plain)
        .env("HOME", &plain)
        .output()
        .unwrap();
    assert!(
        outside.exists(),
        "the unconfined model did not write outside, so this test shows nothing:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_file(&outside).unwrap();

    // In a sandbox, it cannot.
    for (n, sandbox) in sandboxes.iter().enumerate() {
        let what = sandbox.describe();
        let mut job = SimJob::new(deck, dir.path().join(format!("runs-{n}")));
        job.libraries = vec![Arc::new(lib.clone())];
        job.sandbox = Some(sandbox.clone());
        let log = match Ngspice::default()
            .run(&job, &CancellationToken::new())
            .await
        {
            Ok(o) if !o.datasets.is_empty() => o.log,
            Ok(o) => panic!("{what}: ngspice simulated nothing:\n{}", o.log),
            Err(e) => panic!("{what}: {e}"),
        };
        assert!(
            !log.to_ascii_lowercase()
                .contains("failed to load simulation binary"),
            "{what}: the model did not load, so this shows nothing:\n{log}"
        );
        assert!(!outside.exists(), "{what}: the model wrote outside:\n{log}");
    }
}
