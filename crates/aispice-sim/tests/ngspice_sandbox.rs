//! ngspice confined while it runs code a user compiled (Linux only).
//!
//! A probe library, built here from C and handed to the backend as if
//! aispice had compiled it from a project's Verilog-A, runs as soon as
//! ngspice loads it. It tries what the confinement must stop (a socket of
//! every family, a signal to a sibling of ngspice and to its parent, a file
//! outside the run folder) and what it must allow (a file in the run
//! folder, a signal to itself), and prints what happened. The confined runs
//! use Landlock ABI 4, the oldest accepted, where seccomp alone keeps
//! sockets and signals in, and the kernel's own ABI when it scopes them
//! itself. The same probe run in an unconfined ngspice reaches everything,
//! so a probe that silently did nothing cannot pass. Skips without
//! ngspice, a C compiler, or a kernel that can confine.

#![cfg(target_os = "linux")]

use aispice_sim::backend::{Ngspice, SimError, SimJob, Simulator, native_code, which};
use aispice_sim::behavioral::{Library, LibraryKind, sha256_hex};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

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

static void report(const char *what, long r) {
    fprintf(stderr, "PROBE %s %s %d\n", what, r < 0 ? "refused" : "allowed", r < 0 ? errno : 0);
}

__attribute__((constructor)) static void probe(void) {
    fprintf(stderr, "PROBE start\n");
    int fd = open("inside.txt", O_CREAT | O_WRONLY | O_TRUNC, 0600);
    report("write-inside", fd);
    if (fd >= 0) close(fd);
    fd = open(OUTSIDE, O_CREAT | O_WRONLY | O_TRUNC, 0600);
    report("write-outside", fd);
    if (fd >= 0) close(fd);
    int types[] = {SOCK_STREAM, SOCK_DGRAM, SOCK_RAW, SOCK_SEQPACKET};
    for (int family = 0; family < 64; family++) {
        for (int t = 0; t < 4; t++) {
            int s = socket(family, types[t], 0);
            if (s >= 0) {
                fprintf(stderr, "PROBE socket-%d-%d allowed 0\n", family, types[t]);
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
    report("kill-parent", kill(getppid(), 0));
    report("kill-everyone", kill(-1, 0));
    report("kill-self", kill(getpid(), 0));
    fprintf(stderr, "PROBE end\n");
}
"#;

/// The attempts that must fail in a confined run. `kill(-1)` returns 0
/// whenever it had a target, even when every target refused, so with
/// Landlock's signal scope alone it reads as allowed while it reaches
/// nothing; the seccomp policy every confined run has refuses it outright.
const MUST_FAIL: &[&str] = &[
    "write-outside",
    "kill-sibling",
    "tkill-sibling",
    "tgkill-sibling",
    "sigqueue-sibling",
    "pidfd-signal-sibling",
    "kill-parent",
    "kill-everyone",
];

/// What the confined probe sends its sibling: a signal that would end it.
const SIGTERM: i32 = 15;

const DECK: &str = "* probe\nV1 a 0 1\nR1 a 0 1k\n.op\n.end\n";

fn compiler() -> Option<PathBuf> {
    which("cc").or_else(|| which("gcc"))
}

/// The probe as a library aispice "built": it writes `outside` and sends
/// `signal` to `sibling`.
fn probe(cc: &Path, dir: &Path, outside: &Path, sibling: u32, signal: i32) -> Library {
    let src = dir.join("probe.c");
    let so = dir.join(format!("probe-{signal}.so"));
    std::fs::write(&src, PROBE).unwrap();
    let status = Command::new(cc)
        .args(["-shared", "-fPIC", "-O0", "-o"])
        .arg(&so)
        .arg(format!("-DOUTSIDE=\"{}\"", outside.display()))
        .arg(format!("-DSIBLING={sibling}"))
        .arg(format!("-DSIGNAL={signal}"))
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

/// ngspice's output with the probe loaded through the backend, which
/// confines it, writing its ruleset against Landlock ABI `abi`.
async fn confined(lib: &Library, abi: u8, run_dir: &Path) -> String {
    let mut job = SimJob::new(DECK, run_dir);
    job.libraries = vec![Arc::new(lib.clone())];
    let ngspice = Ngspice {
        landlock_abi: Some(abi),
        ..Default::default()
    };
    // ngspice reports the probe as a broken OSDI library and runs on. A run
    // without results stopped early, and would show nothing about what a
    // working simulation can reach.
    match ngspice.run(&job, &CancellationToken::new()).await {
        Ok(out) if !out.datasets.is_empty() => out.log,
        Ok(out) => panic!("ABI {abi}: ngspice simulated nothing:\n{}", out.log),
        Err(SimError::Failed { log, .. }) => panic!("ABI {abi}: ngspice failed:\n{log}"),
        Err(e) => panic!("ABI {abi}: the confined run did not start: {e}"),
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

/// The tools and the Landlock ABIs to test, or why the tests skip.
fn setup() -> Option<(PathBuf, PathBuf, Vec<u8>)> {
    let (Some(exe), Some(cc)) = (which("ngspice"), compiler()) else {
        eprintln!("skipped: needs ngspice and a C compiler");
        return None;
    };
    let kernel = match native_code::available() {
        Ok(abi) => abi,
        Err(e) => {
            eprintln!("skipped: {e}");
            return None;
        }
    };
    let mut abis = vec![4];
    if kernel > 4 {
        abis.push(kernel);
    }
    Some((exe, cc, abis))
}

#[tokio::test(flavor = "multi_thread")]
async fn compiled_code_in_a_confined_ngspice_reaches_no_socket_signal_or_outside_file() {
    let Some((_, cc, abis)) = setup() else {
        return;
    };
    for abi in abis {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("escaped.txt");
        let mut sibling = Sibling::start();
        let lib = probe(&cc, dir.path(), &outside, sibling.pid(), SIGTERM);
        let log = confined(&lib, abi, &dir.path().join("runs")).await;
        assert!(
            log.contains("PROBE start") && log.contains("PROBE end"),
            "ABI {abi}: the probe did not run to the end:\n{log}"
        );
        assert_eq!(
            allowed(&log, "write-inside"),
            Some(true),
            "ABI {abi}:\n{log}"
        );
        assert_eq!(allowed(&log, "kill-self"), Some(true), "ABI {abi}:\n{log}");
        for what in MUST_FAIL {
            assert_ne!(allowed(&log, what), Some(true), "ABI {abi}: {what}:\n{log}");
        }
        let sockets: Vec<String> = attempts(&log)
            .into_iter()
            .filter(|(w, _)| w.starts_with("socket-"))
            .map(|(w, _)| w)
            .collect();
        assert!(sockets.is_empty(), "ABI {abi}: sockets made: {sockets:?}");
        assert!(!outside.exists(), "ABI {abi}: a file was written outside");
        assert!(sibling.alive(), "ABI {abi}: the sibling was killed");
    }
}

/// The same probe, unconfined, does everything the confined runs refuse:
/// the test above would see an escape.
#[tokio::test(flavor = "multi_thread")]
async fn the_probe_reaches_all_of_it_when_ngspice_runs_unconfined() {
    let Some((exe, cc, _)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("escaped.txt");
    let mut sibling = Sibling::start();
    // Signal 0 only checks that a signal could be sent.
    let lib = probe(&cc, dir.path(), &outside, sibling.pid(), 0);
    let log = unconfined(&exe, &lib);
    for what in [
        "write-inside",
        "write-outside",
        "kill-sibling",
        "kill-parent",
        "kill-self",
    ] {
        assert_eq!(allowed(&log, what), Some(true), "{what}:\n{log}");
    }
    assert!(
        attempts(&log)
            .iter()
            .any(|(w, a)| w.starts_with("socket-") && *a),
        "no socket was made:\n{log}"
    );
    assert!(outside.exists());
    assert!(sibling.alive());
}

/// Below ABI 4 the libraries are not loaded and ngspice does not start.
#[tokio::test(flavor = "multi_thread")]
async fn below_landlock_4_compiled_code_is_not_loaded() {
    let Some((_, cc, _)) = setup() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("escaped.txt");
    let lib = probe(&cc, dir.path(), &outside, std::process::id(), 0);
    let mut job = SimJob::new(DECK, dir.path().join("runs"));
    job.libraries = vec![Arc::new(lib)];
    let ngspice = Ngspice {
        landlock_abi: Some(3),
        ..Default::default()
    };
    let e = ngspice
        .run(&job, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(e.to_string().contains("ABI 3"), "{e}");
    assert!(!outside.exists());
}
