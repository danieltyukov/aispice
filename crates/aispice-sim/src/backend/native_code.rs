//! Confining ngspice while it runs code a user compiled.
//!
//! A project's Verilog-A becomes an OSDI library and its Verilog a
//! `d_cosim` library, and ngspice loads both into its own process, so
//! whatever the source said runs there. The source checks
//! ([`crate::behavioral`]) refuse what a model has no business doing; this
//! confinement holds whatever gets past them. ngspice then:
//!
//! - reads the system (`/usr`, the loader's files), its own install, the
//!   folders the netlist policy lets the deck include from (the project,
//!   the model folders, the registered PDKs) and the run folder, and from
//!   `/proc` only `meminfo` for its memory report;
//! - executes only itself and the dynamic loader;
//! - writes only the run folder. ngspice copies its input into `tmpfile()`
//!   in every batch run, and glibc makes that file in `/tmp` whatever
//!   `TMPDIR` says, so aispice builds a small library from a fixed C source
//!   ([`TMPFILE_SOURCE`]) and preloads it, which makes the file in the run
//!   folder instead;
//! - makes no socket of any family and signals nothing but itself and its
//!   process group: seccomp's native-code profile, and from Landlock ABI 6
//!   also Landlock's own scopes;
//! - starts from a fixed environment, so nothing aispice holds there (an API
//!   key, a token) is readable;
//! - and is limited in file size, address space and processes.
//!
//! Landlock ABI 4 (Linux 6.7) is the oldest accepted: below it a kernel
//! cannot refuse TCP or truncation. Without Landlock, seccomp filters or a
//! C compiler, and on every system but Linux, the libraries are not loaded
//! at all; there is no unconfined fallback.

use super::process::{self, ProcessSpec};
use super::{RAW_SIZE_LIMIT, SimError, which};
use crate::sandbox::policy::MAX_LANDLOCK_ABI;
use crate::sandbox::{
    Confinement, LandlockPolicy, Limits, MIN_LANDLOCK_ABI, Profile, SCOPED_LANDLOCK_ABI, Scopes,
    SeccompPolicy, Signals,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The largest file a confined run may write: twice what aispice reads back
/// as a raw file, so an oversized result still gets the readable "too
/// large" error rather than a SIGXFSZ.
const MAX_FILE: u64 = 2 * RAW_SIZE_LIMIT;

/// The address space of a confined run. ngspice starts a thread per core
/// and each reserves a stack and a malloc arena: about 1.4 GB of address
/// space on 22 cores for a small co-simulation, well under this.
const MAX_MEMORY: u64 = 8 << 30;

/// System paths every confined run reads: programs and shared libraries
/// under `/usr`, the loader's cache and configuration, `/dev/urandom` for
/// runtimes that seed from it, and `/proc/meminfo` for ngspice's memory
/// report. `/proc` itself is not readable: it would show other processes'
/// command lines. ngspice's report of its own size then fails with one
/// `fopen("/proc/%d/statm")` line in the log, which is harmless.
const SYSTEM: &[&str] = &[
    "/usr",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/dev/urandom",
    "/proc/meminfo",
];

/// Top-level folders that are links into `/usr` on most systems (already
/// readable through it) and real folders on some.
const SYSTEM_MAYBE_LINKS: &[&str] = &["/bin", "/lib", "/lib64", "/lib32", "/libx32"];

/// Dynamic loaders, which the kernel opens for execution as every binary's
/// interpreter.
const LOADERS: &[&str] = &["/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-aarch64.so.1"];

/// The library preloaded into a confined run: `tmpfile()` (the only
/// temporary-file function ngspice imports) made in `TMPDIR`, the run
/// folder, as an unnamed file, or on a filesystem without `O_TMPFILE` as a
/// named one removed at once.
pub const TMPFILE_SOURCE: &str = r#"#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

FILE *tmpfile(void)
{
    const char *dir = getenv("TMPDIR");
    if (!dir || !*dir)
        dir = ".";
    int fd = open(dir, O_RDWR | O_TMPFILE | O_EXCL | O_CLOEXEC, 0600);
    if (fd < 0) {
        char path[4096];
        if (snprintf(path, sizeof path, "%s/tmpfXXXXXX", dir) >= (int) sizeof path)
            return NULL;
        fd = mkostemp(path, O_CLOEXEC);
        if (fd < 0)
            return NULL;
        unlink(path);
    }
    FILE *f = fdopen(fd, "w+");
    if (!f)
        close(fd);
    return f;
}

FILE *tmpfile64(void) __attribute__((alias("tmpfile")));
"#;

/// The preloaded library's file name in a run folder.
const TMPFILE_LIBRARY: &str = "aispice_tmpfile.so";

/// The preloaded library, built once per process.
static TMPFILE_BYTES: OnceLock<Arc<[u8]>> = OnceLock::new();

/// A C compiler for the preloaded library.
fn c_compiler() -> Option<PathBuf> {
    which("cc")
        .or_else(|| which("gcc"))
        .or_else(|| which("clang"))
}

/// The Landlock ABI a confined run will be written against, or why user
/// compiled code cannot be loaded on this machine.
#[cfg(target_os = "linux")]
pub fn available() -> Result<u8, String> {
    let seccomp = Path::new("/proc/sys/kernel/seccomp/actions_avail").exists();
    let abi = choose_abi(super::confine::abi(), seccomp)?;
    if c_compiler().is_none() {
        return Err("aispice needs a C compiler (cc) to build the small library that keeps a confined ngspice's temporary file in its run folder".into());
    }
    Ok(abi)
}

#[cfg(not(target_os = "linux"))]
pub fn available() -> Result<u8, String> {
    Err("aispice loads compiled Verilog-A and Verilog models only on Linux, where it can confine ngspice with Landlock and seccomp".into())
}

/// The ABI to write the ruleset against for a kernel with Landlock ABI
/// `kernel` (0 when it has none): at least [`MIN_LANDLOCK_ABI`], at most
/// the newest aispice knows.
fn choose_abi(kernel: i32, seccomp: bool) -> Result<u8, String> {
    if !seccomp {
        return Err(
            "the kernel has no seccomp filters, which aispice needs to confine ngspice while it runs compiled models".into(),
        );
    }
    match kernel {
        0 => Err("the kernel has no Landlock, which aispice needs to confine ngspice while it runs compiled models".into()),
        k if k < i32::from(MIN_LANDLOCK_ABI) => Err(format!(
            "the kernel's Landlock ABI {k} is below {MIN_LANDLOCK_ABI} (Linux 6.7), which aispice needs to keep compiled models off the network"
        )),
        k => Ok(k.min(i32::from(MAX_LANDLOCK_ABI)) as u8),
    }
}

/// The confinement for ngspice at `exe` running in the private folder
/// `run`, which it may write; `reads` are the folders the deck may include
/// from and `files` single files it loads (a PDK's OSDI models). `abi` is
/// the Landlock ABI to write the ruleset against, from [`available`] or
/// lower.
pub(crate) fn confinement(
    exe: &Path,
    run: &Path,
    reads: &[PathBuf],
    files: &[PathBuf],
    abi: u8,
) -> Result<Confinement, SimError> {
    let real = |p: &Path| {
        p.canonicalize()
            .map_err(|e| SimError::Io(format!("cannot resolve {}: {e}", p.display())))
    };
    let exe = real(exe)?;
    let run = real(run)?;
    let mut read: Vec<PathBuf> = SYSTEM.iter().map(PathBuf::from).collect();
    read.extend(
        SYSTEM_MAYBE_LINKS
            .iter()
            .map(Path::new)
            .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir()))
            .map(Path::to_path_buf),
    );
    read.extend(install_dirs(&exe));
    // A folder or file that does not exist has nothing to read.
    read.extend(
        reads
            .iter()
            .chain(files)
            .filter_map(|p| p.canonicalize().ok()),
    );
    read.push(exe.clone());
    read.push(run.clone());
    read.sort();
    read.dedup();
    let mut exec = vec![exe];
    for l in LOADERS {
        if let Ok(p) = Path::new(l).canonicalize()
            && !exec.contains(&p)
        {
            exec.push(p);
        }
    }
    let scopes = if abi >= SCOPED_LANDLOCK_ABI {
        Scopes::ALL
    } else {
        Scopes::default()
    };
    Confinement::builder()
        .landlock(LandlockPolicy {
            read,
            exec,
            write: vec![run],
            rw: vec![PathBuf::from("/dev/null")],
            min_abi: abi,
            scopes,
        })
        .seccomp(SeccompPolicy {
            profile: Profile::NativeCode,
            signals: Signals::SelfOnly,
        })
        .limits(Limits {
            file_size: Some(MAX_FILE),
            address_space: Some(MAX_MEMORY),
            nproc_headroom: Some(nproc_headroom()),
        })
        .build()
        .map_err(|e| {
            SimError::Config(format!(
                "ngspice cannot be confined to run the project's compiled models: {e}"
            ))
        })
}

/// The environment of a confined run: these variables and nothing of
/// aispice's own. `preload` is the placed [`TMPFILE_SOURCE`] library.
pub(crate) fn environment(run: &Path, preload: &Path) -> Vec<(OsString, OsString)> {
    vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), run.into()),
        ("TMPDIR".into(), run.into()),
        ("LC_ALL".into(), "C".into()),
        ("LD_PRELOAD".into(), preload.into()),
    ]
}

/// The [`TMPFILE_SOURCE`] library, built the first time a confined run
/// needs it.
pub(crate) async fn tmpfile_library(cancel: &CancellationToken) -> Result<Arc<[u8]>, SimError> {
    if let Some(b) = TMPFILE_BYTES.get() {
        return Ok(b.clone());
    }
    let cc = c_compiler().ok_or_else(|| {
        SimError::NotFound("a C compiler (cc), which a confined ngspice run needs".into())
    })?;
    let dir = tempfile::tempdir()
        .map_err(|e| SimError::Io(format!("cannot make a build folder: {e}")))?;
    std::fs::write(dir.path().join("tmpfile.c"), TMPFILE_SOURCE)
        .map_err(|e| SimError::Io(format!("cannot write tmpfile.c: {e}")))?;
    let spec = ProcessSpec::new(&cc, dir.path())
        .arg("-shared")
        .arg("-fPIC")
        .arg("-O2")
        .arg("-o")
        .arg(TMPFILE_LIBRARY)
        .arg("tmpfile.c");
    let out = process::run(&spec, Duration::from_secs(60), cancel).await?;
    if !out.success() {
        return Err(SimError::Config(format!(
            "{} could not build the library a confined ngspice run preloads: {}",
            cc.display(),
            out.combined().trim()
        )));
    }
    let bytes: Arc<[u8]> = std::fs::read(dir.path().join(TMPFILE_LIBRARY))
        .map_err(|e| SimError::Io(format!("cannot read {TMPFILE_LIBRARY}: {e}")))?
        .into();
    Ok(TMPFILE_BYTES.get_or_init(|| bytes).clone())
}

/// Write the preloaded library into the run folder and check the file
/// holds exactly its bytes. `LD_PRELOAD` splits at spaces and colons, so a
/// run folder with either in its path cannot hold it.
pub(crate) fn place_tmpfile_library(run: &Path, bytes: &[u8]) -> Result<PathBuf, SimError> {
    let path = run.join(TMPFILE_LIBRARY);
    if path.to_string_lossy().contains([' ', ':']) {
        return Err(SimError::Config(format!(
            "the temporary folder {} has a space or a colon in its path, which a confined ngspice run cannot preload from; set TMPDIR to a plain path",
            run.display()
        )));
    }
    std::fs::write(&path, bytes)
        .map_err(|e| SimError::Io(format!("cannot write {}: {e}", path.display())))?;
    let written = std::fs::read(&path)
        .map_err(|e| SimError::Io(format!("cannot read back {}: {e}", path.display())))?;
    if written != bytes {
        return Err(SimError::Config(format!(
            "{} does not hold the library aispice built; it was not loaded",
            path.display()
        )));
    }
    Ok(path)
}

/// The folders of ngspice's own install that hold its scripts (`spinit`)
/// and code models (`*.cm`), for an install outside `/usr`: beside
/// `bin/ngspice`, under `share/ngspice` and `lib/ngspice` (or a multiarch
/// `lib/<triple>/ngspice`). Only these, never the prefix itself, which for
/// `~/bin/ngspice` would be the home folder.
fn install_dirs(exe: &Path) -> Vec<PathBuf> {
    let Some(prefix) = exe.parent().and_then(Path::parent) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = ["share/ngspice", "lib/ngspice", "lib64/ngspice"]
        .iter()
        .map(|s| prefix.join(s))
        .collect();
    if let Ok(entries) = std::fs::read_dir(prefix.join("lib")) {
        v.extend(entries.flatten().map(|e| e.path().join("ngspice")));
    }
    v.retain(|p| p.is_dir());
    v
}

/// How many processes and threads a confined run may add to what its user
/// runs: ngspice starts a thread per core, and a little more.
fn nproc_headroom() -> u64 {
    let cores = std::thread::available_parallelism().map_or(8, |n| n.get() as u64);
    64 + 2 * cores
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_kernel_must_have_landlock_4_and_seccomp() {
        assert!(choose_abi(0, true).unwrap_err().contains("no Landlock"));
        for k in 1..4 {
            let e = choose_abi(k, true).unwrap_err();
            assert!(e.contains(&format!("ABI {k} is below 4")), "{e}");
        }
        assert!(choose_abi(8, false).unwrap_err().contains("seccomp"));
        assert_eq!(choose_abi(4, true), Ok(4));
        assert_eq!(choose_abi(6, true), Ok(6));
        // A newer kernel gets the newest ruleset aispice can write.
        assert_eq!(choose_abi(8, true), Ok(MAX_LANDLOCK_ABI));
    }

    fn fake_ngspice(dir: &Path) -> PathBuf {
        let bin = dir.join("opt/ngspice/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(dir.join("opt/ngspice/share/ngspice/scripts")).unwrap();
        std::fs::create_dir_all(dir.join("opt/ngspice/lib/ngspice")).unwrap();
        let exe = bin.join("ngspice");
        std::fs::write(&exe, b"").unwrap();
        exe
    }

    #[test]
    fn ngspice_writes_only_its_run_folder_and_executes_only_itself() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_ngspice(dir.path());
        let run = dir.path().join("run");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let gone = dir.path().join("no-such-folder");
        let c = confinement(&exe, &run, &[project.clone(), gone.clone()], &[], 6).unwrap();
        let ll = c.landlock().unwrap();
        let real = |p: &Path| p.canonicalize().unwrap();
        assert_eq!(ll.write, vec![real(&run)]);
        assert_eq!(ll.rw, vec![PathBuf::from("/dev/null")]);
        assert_eq!(ll.exec[0], real(&exe));
        assert!(
            ll.exec[1..]
                .iter()
                .all(|p| p.to_string_lossy().contains("ld-linux")),
            "{:?}",
            ll.exec
        );
        for p in [real(&project), real(&run), PathBuf::from("/usr")] {
            assert!(ll.read.contains(&p), "{p:?} in {:?}", ll.read);
        }
        // Its install's scripts and code models, never the prefix itself.
        let prefix = real(&dir.path().join("opt/ngspice"));
        assert!(ll.read.contains(&prefix.join("share/ngspice")));
        assert!(ll.read.contains(&prefix.join("lib/ngspice")));
        assert!(!ll.read.contains(&prefix));
        assert!(!ll.read.iter().any(|p| p.ends_with("no-such-folder")));
        // From /proc, only the one global file.
        assert!(
            ll.read
                .iter()
                .filter(|p| p.starts_with("/proc"))
                .all(|p| p == Path::new("/proc/meminfo"))
        );
    }

    #[test]
    fn below_abi_6_seccomp_holds_signals_and_sockets() {
        let dir = tempfile::tempdir().unwrap();
        let exe = fake_ngspice(dir.path());
        for abi in [4, 5, 6] {
            let c = confinement(&exe, dir.path(), &[], &[], abi).unwrap();
            let ll = c.landlock().unwrap();
            assert_eq!(ll.min_abi, abi);
            let want = if abi >= 6 {
                Scopes::ALL
            } else {
                Scopes::default()
            };
            assert_eq!(ll.scopes, want);
            assert_eq!(
                c.seccomp(),
                Some(&SeccompPolicy {
                    profile: Profile::NativeCode,
                    signals: Signals::SelfOnly,
                })
            );
            let lim = c.limits();
            assert_eq!(lim.file_size, Some(MAX_FILE));
            assert_eq!(lim.address_space, Some(MAX_MEMORY));
            assert!(lim.nproc_headroom.is_some());
        }
        let e = confinement(&exe, dir.path(), &[], &[], 3).unwrap_err();
        assert!(e.to_string().contains("ABI 3"), "{e}");
    }

    /// The sentinel: Verilog the source check refuses (`$fopen` of a path
    /// outside the run), built around the check as if the check were wrong.
    /// Unconfined, ngspice running it writes the file; confined, at ABI 4
    /// and at the kernel's, it does not. Skips without the tools or a
    /// kernel that can confine.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_the_source_check_refuses_still_cannot_write_outside_its_run() {
        use crate::backend::ngspice::with_cosim_models;
        use crate::backend::{Ngspice, SimJob, Simulator, which};
        use crate::behavioral::{Source, verilog};
        use std::sync::Arc;
        use tokio_util::sync::CancellationToken;

        let (Some(ngspice), Some(verilator), Some(cxx)) =
            (which("ngspice"), which("verilator"), verilog::cxx())
        else {
            eprintln!("skipped: needs ngspice, Verilator and a C++ compiler");
            return;
        };
        let Some(shim) = verilog::shim_dir(&ngspice) else {
            eprintln!("skipped: ngspice has no d_cosim sources");
            return;
        };
        let kernel = match available() {
            Ok(abi) => abi,
            Err(e) => {
                eprintln!("skipped: {e}");
                return;
            }
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
        let out = std::process::Command::new(&ngspice)
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

        // Confined, it cannot.
        let mut abis = vec![4];
        if kernel > 4 {
            abis.push(kernel);
        }
        for abi in abis {
            let mut job = SimJob::new(deck, dir.path().join(format!("runs-{abi}")));
            job.libraries = vec![Arc::new(lib.clone())];
            let ngspice = Ngspice {
                landlock_abi: Some(abi),
                ..Default::default()
            };
            // A run without results stopped before the model ran.
            let log = match ngspice.run(&job, &CancellationToken::new()).await {
                Ok(o) if !o.datasets.is_empty() => o.log,
                Ok(o) => panic!("ABI {abi}: ngspice simulated nothing:\n{}", o.log),
                Err(e) => panic!("ABI {abi}: {e}"),
            };
            assert!(
                !log.to_ascii_lowercase()
                    .contains("failed to load simulation binary"),
                "ABI {abi}: the model did not load, so this shows nothing:\n{log}"
            );
            assert!(
                !outside.exists(),
                "ABI {abi}: the model wrote outside:\n{log}"
            );
        }
    }

    #[test]
    fn the_preloaded_library_needs_a_plain_run_folder() {
        let dir = tempfile::tempdir().unwrap();
        for bad in ["with space", "with:colon"] {
            let run = dir.path().join(bad);
            std::fs::create_dir(&run).unwrap();
            let e = place_tmpfile_library(&run, b"lib").unwrap_err();
            assert!(e.to_string().contains("space or a colon"), "{e}");
            assert!(!run.join(TMPFILE_LIBRARY).exists());
        }
        let p = place_tmpfile_library(dir.path(), b"lib").unwrap();
        assert_eq!(std::fs::read(p).unwrap(), b"lib");
    }

    #[test]
    fn a_confined_run_inherits_nothing() {
        let env = environment(
            Path::new("/tmp/run"),
            Path::new("/tmp/run/aispice_tmpfile.so"),
        );
        let keys: Vec<_> = env.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        assert_eq!(keys, ["PATH", "HOME", "TMPDIR", "LC_ALL", "LD_PRELOAD"]);
        assert!(
            env.iter()
                .filter(|(k, _)| k == "HOME" || k == "TMPDIR")
                .all(|(_, v)| v == "/tmp/run")
        );
    }
}
