//! Running ngspice while it loads code a user compiled.
//!
//! A project's Verilog-A becomes an OSDI library and its Verilog a
//! `d_cosim` library, and ngspice loads both into its own process, so
//! whatever the source said runs there. The source checks
//! ([`crate::behavioral`]) refuse what a model has no business doing; the
//! sandbox ([`crate::sandbox`]) holds whatever gets past them. ngspice runs
//! there under seccomp's [`Profile::NativeCode`] and:
//!
//! - reads the system, its own install, the files the deck includes and
//!   names as the netlist policy resolved them, the project's PDK and the
//!   run folder, and nothing else of the project: not the project folder,
//!   which holds aispice's own state under `.aispice/`;
//! - executes only itself and the dynamic loader;
//! - writes only the run folder. ngspice copies its input into `tmpfile()`
//!   in every batch run, and glibc makes that file in `/tmp` whatever
//!   `TMPDIR` says, so aispice builds a small library from a fixed C source
//!   ([`TMPFILE_SOURCE`]) and preloads it, which makes the file in the run
//!   folder and never in `/tmp`;
//! - makes no socket and signals nothing outside the sandbox;
//! - starts from a fixed environment ([`environment`]), so nothing aispice
//!   holds there (an API key, a token) is readable, and `LD_PRELOAD` names
//!   that library and nothing else: no deck, `.spiceinit` or project file
//!   can set a variable;
//! - and is limited in file size, address space and processes.
//!
//! The preloaded library is built with the system's C compiler, found only
//! in [`TOOL_PATH`] and run with fixed flags and an environment of its own,
//! and cached in aispice's cache folder under a key made of the source's
//! hash and the compiler's path and version, with its SHA-256. A cached
//! copy is used only when it still has that SHA-256, and every copy placed
//! in a run folder is read back and checked against it.
//!
//! Where no sandbox can start (not Linux, no seccomp, neither bubblewrap
//! nor Landlock ABI 4) or there is no C compiler, the libraries are not
//! loaded at all, and [`ready`] says what is missing and how to simulate
//! without them; there is no unconfined fallback.

use super::process::{self, ProcessSpec};
use super::{RAW_SIZE_LIMIT, SimError};
use crate::behavioral::sha256_hex;
use crate::sandbox::{Limits, Plan, Profile, Sandbox};
use aispice_core::netlist::LinkedInclude;
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

/// The library preloaded into a confined run: `tmpfile()` and
/// `tmpfile64()` (the only temporary-file functions ngspice imports) made
/// in `TMPDIR`, the run folder, as an unnamed file, or on a filesystem
/// without `O_TMPFILE` as a named one removed at once. Without an absolute
/// `TMPDIR`, or when neither works there, it returns `NULL` with `errno`
/// set, as `tmpfile()` does on failure; it never uses `/tmp`.
pub const TMPFILE_SOURCE: &str = r#"#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

FILE *tmpfile(void)
{
    const char *dir = getenv("TMPDIR");
    if (!dir || dir[0] != '/') {
        errno = ENOENT;
        return NULL;
    }
    int fd = open(dir, O_RDWR | O_TMPFILE | O_EXCL | O_CLOEXEC, 0600);
    if (fd < 0) {
        char path[4096];
        int n = snprintf(path, sizeof path, "%s/tmpfXXXXXX", dir);
        if (n < 0 || n >= (int) sizeof path) {
            errno = ENAMETOOLONG;
            return NULL;
        }
        fd = mkostemp(path, O_CLOEXEC);
        if (fd < 0)
            return NULL;
        unlink(path);
    }
    FILE *f = fdopen(fd, "w+");
    if (!f) {
        int e = errno;
        close(fd);
        errno = e;
    }
    return f;
}

FILE *tmpfile64(void) __attribute__((alias("tmpfile")));
"#;

/// The preloaded library's file name in a run folder and in the cache.
pub const PRELOAD_FILE: &str = "aispice_tmpfile.so";

/// The only folders the C compiler is looked for in. aispice's own `PATH`
/// is not used: it may hold the working folder, or a project's folder that
/// a shell hook added.
pub const TOOL_PATH: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];

/// The C compilers tried, in this order.
const COMPILERS: &[&str] = &["cc", "gcc", "clang"];

/// The arguments the compiler is run with, and nothing else.
const COMPILER_ARGS: &[&str] = &["-O2", "-fPIC", "-shared", "-o", PRELOAD_FILE, "tmpfile.c"];

/// The built library is a few kilobytes; nothing larger is read back.
const MAX_PRELOAD_BYTES: u64 = 1 << 20;

/// How long building the library may take.
const BUILD_TIMEOUT: Duration = Duration::from_secs(60);

/// What to do instead, in every refusal.
const WITHOUT: &str = "use a deck that does not instantiate the models declared under `veriloga` and `[[cosim]]` in aispice.toml; everything else, PDK models included, runs as usual";

/// The C compiler for the preloaded library: `cc`, `gcc` or `clang` in
/// [`TOOL_PATH`].
pub fn compiler() -> Option<PathBuf> {
    COMPILERS.iter().find_map(|name| {
        TOOL_PATH
            .iter()
            .map(|dir| Path::new(dir).join(name))
            .find(|p| super::is_executable(p))
    })
}

/// Whether the kernel has seccomp filters.
fn seccomp_here() -> bool {
    Path::new("/proc/sys/kernel/seccomp/actions_avail").exists()
}

/// The sandbox a run that loads compiled models will use, or what this
/// machine lacks for one and how to simulate without them.
pub async fn ready(cancel: &CancellationToken) -> Result<Sandbox, String> {
    ready_for(Purpose::CompiledModels, cancel).await
}

/// Why a run needs the sandboxed ngspice: it loads the project's compiled
/// models, or it has control commands (allowed by the user's setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    CompiledModels,
    ControlCommands,
}

/// [`ready`] for `purpose`, whose refusal says what was not run and how to
/// run without it.
pub async fn ready_for(purpose: Purpose, cancel: &CancellationToken) -> Result<Sandbox, String> {
    let sandbox = if !cfg!(target_os = "linux") {
        Err("this system is not Linux".to_string())
    } else if !seccomp_here() {
        Err("the kernel has no seccomp filters".to_string())
    } else {
        Sandbox::select(cancel).await
    };
    let no_compiler = compiler().is_none();
    match sandbox {
        Ok(s) if !no_compiler => Ok(s),
        Ok(_) => Err(refusal_for(purpose, None, true)),
        Err(why) => Err(refusal_for(purpose, Some(&why), no_compiler)),
    }
}

/// Refuse, under bubblewrap, a deck that includes a file through a symbolic
/// link outside the folders the sandbox shows whole (`whole`, such as the
/// project's PDK, and the system). bubblewrap shows every other file the
/// deck reads at its resolved path only, so ngspice could not open the
/// link and would stop with an include error; this names the include, the
/// link and the fix instead. Landlock grants the file itself, which ngspice
/// reaches through the link, so it needs no such check.
pub fn check_links(
    sandbox: &Sandbox,
    linked: &[LinkedInclude],
    whole: &[PathBuf],
) -> Result<(), String> {
    if !sandbox.is_bwrap() {
        return Ok(());
    }
    for l in linked {
        let link = first_link(&l.path).unwrap_or_else(|| l.path.clone());
        let shown = |d: &Path| link.starts_with(d.canonicalize().unwrap_or_else(|_| d.into()));
        if shown(Path::new("/usr")) || whole.iter().any(|d| shown(d)) {
            continue;
        }
        return Err(format!(
            "the include `{}` reaches {} through the symbolic link {}, which ngspice cannot follow in the bubblewrap sandbox that runs the project's compiled models (it shows each file the deck reads at its real path only). Include {} itself, or replace the link with the file.",
            l.include,
            l.file.display(),
            link.display(),
            l.file.display()
        ));
    }
    Ok(())
}

/// The first symbolic link on the way from `/` to `p`, if any.
fn first_link(p: &Path) -> Option<PathBuf> {
    let mut ancestors: Vec<&Path> = p.ancestors().collect();
    ancestors.reverse();
    ancestors
        .into_iter()
        .find(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()))
        .map(Path::to_path_buf)
}

/// The message for a run whose compiled models are not loaded: what is
/// missing (the sandbox and why, the C compiler) and what to do instead.
fn refusal(no_sandbox: Option<&str>, no_compiler: bool) -> String {
    refusal_for(Purpose::CompiledModels, no_sandbox, no_compiler)
}

/// [`refusal`] for `purpose`.
fn refusal_for(purpose: Purpose, no_sandbox: Option<&str>, no_compiler: bool) -> String {
    if purpose == Purpose::ControlCommands {
        let mut missing = Vec::new();
        if let Some(why) = no_sandbox {
            missing.push(format!(
                "{why} (the sandbox needs Linux with seccomp and either bubblewrap 0.8 or later or Landlock ABI 4 (Linux 6.7) or later; see docs/SANDBOX.md)"
            ));
        }
        if no_compiler {
            missing.push(format!(
                "there is no C compiler ({} in {}), which the sandboxed ngspice needs",
                COMPILERS.join(", "),
                TOOL_PATH.join(", ")
            ));
        }
        return format!(
            "ngspice was not run: control blocks are enabled in the settings, and aispice then runs every ngspice simulation in a sandbox, but {}. To simulate here, turn control blocks off in the settings (a deck's `.control` block and `*#` lines are then refused) and use aispice's measurements instead.",
            missing.join("; and ")
        );
    }
    let mut missing = Vec::new();
    if let Some(why) = no_sandbox {
        missing.push(format!(
            "{why}, and they run only in a sandboxed ngspice, which needs Linux with seccomp and either bubblewrap 0.8 or later or Landlock ABI 4 (Linux 6.7) or later (see docs/SANDBOX.md)"
        ));
    }
    if no_compiler {
        missing.push(format!(
            "there is no C compiler ({} in {}), which aispice needs to build the small library that keeps a sandboxed ngspice's temporary file in its run folder",
            COMPILERS.join(", "),
            TOOL_PATH.join(", ")
        ));
    }
    let instead = if no_sandbox.is_none() {
        format!("Install a C compiler (gcc or clang), or to simulate without them, {WITHOUT}.")
    } else {
        format!("To simulate here without them, {WITHOUT}.")
    };
    format!(
        "the project's Verilog-A and co-simulated models were not loaded: {}. {instead}",
        missing.join("; and ")
    )
}

/// The library a confined run preloads, with the SHA-256 of its bytes.
#[derive(Debug, Clone)]
pub struct Preload {
    bytes: Arc<[u8]>,
    sha256: String,
}

impl Preload {
    pub fn sha256(&self) -> &str {
        &self.sha256
    }
}

/// The library this process preloads, built the first time it is needed.
/// There is no disk cache: the shim is a few kilobytes and one `cc` run,
/// and a cache in `~/.cache` would be another file to verify and a place a
/// project could point `XDG_CACHE_HOME` at. It is built once per process
/// and kept here.
fn memo() -> &'static tokio::sync::Mutex<Option<Preload>> {
    static MEMO: OnceLock<tokio::sync::Mutex<Option<Preload>>> = OnceLock::new();
    MEMO.get_or_init(|| tokio::sync::Mutex::new(None))
}

/// The preloaded library: the one this process already built, else a new
/// build.
pub async fn preload(cancel: &CancellationToken) -> Result<Preload, SimError> {
    let mut memo = memo().lock().await;
    if let Some(p) = memo.as_ref() {
        return Ok(p.clone());
    }
    let p = build(cancel).await?;
    *memo = Some(p.clone());
    Ok(p)
}

/// The compiler's environment: nothing of aispice's (no `CFLAGS`,
/// `LDFLAGS`, `CPATH`, `LIBRARY_PATH`, `GCC_EXEC_PREFIX` or `COMPILER_PATH`),
/// [`TOOL_PATH`] to find its assembler and linker, and `dir` for its
/// temporary files.
fn compiler_spec(cc: &Path, dir: &Path, args: &[&str]) -> ProcessSpec {
    let mut spec = ProcessSpec::new(cc, dir);
    spec.args = args.iter().map(OsString::from).collect();
    spec.clear_env = true;
    spec.env = vec![
        ("PATH".into(), TOOL_PATH.join(":").into()),
        ("TMPDIR".into(), dir.into()),
        ("LC_ALL".into(), "C".into()),
    ];
    spec
}

/// Build [`TMPFILE_SOURCE`] with the system C compiler in a private folder.
pub(crate) async fn build(cancel: &CancellationToken) -> Result<Preload, SimError> {
    let cc = compiler().ok_or_else(|| SimError::Config(refusal(None, true)))?;
    let dir = tempfile::Builder::new()
        .prefix("aispice-preload-")
        .tempdir()
        .map_err(|e| SimError::Io(format!("cannot make a build folder: {e}")))?;
    std::fs::write(dir.path().join("tmpfile.c"), TMPFILE_SOURCE)
        .map_err(|e| SimError::Io(format!("cannot write tmpfile.c: {e}")))?;
    let out = process::run(
        &compiler_spec(&cc, dir.path(), COMPILER_ARGS),
        BUILD_TIMEOUT,
        cancel,
    )
    .await?;
    if !out.success() {
        return Err(SimError::Config(format!(
            "{} could not build the library a sandboxed ngspice preloads: {}",
            cc.display(),
            out.combined().trim()
        )));
    }
    let bytes = read_plain(&dir.path().join(PRELOAD_FILE), MAX_PRELOAD_BYTES).ok_or_else(|| {
        SimError::Config(format!("{} wrote no usable {PRELOAD_FILE}", cc.display()))
    })?;
    Ok(Preload {
        sha256: sha256_hex(&bytes),
        bytes: bytes.into(),
    })
}

/// A file's bytes, only when it is a plain file (not a link) that not every
/// user may write, of at most `limit` bytes. Read through one descriptor.
#[cfg(unix)]
fn read_plain(p: &Path, limit: u64) -> Option<Vec<u8>> {
    use std::io::Read as _;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(p)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.permissions().mode() & 0o002 != 0 || meta.len() > limit {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

#[cfg(not(unix))]
fn read_plain(_: &Path, _: u64) -> Option<Vec<u8>> {
    None
}

/// Write the preloaded library into the run folder `run` as a new file
/// only its owner may write, and check, by reading it back, that it holds
/// exactly the bytes recorded when it was built.
pub(crate) fn place_preload(run: &Path, p: &Preload) -> Result<PathBuf, SimError> {
    use std::io::Write as _;
    let path = run.join(PRELOAD_FILE);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(&path)
        .and_then(|mut f| f.write_all(&p.bytes))
        .map_err(|e| SimError::Io(format!("cannot write {}: {e}", path.display())))?;
    let written = read_plain(&path, MAX_PRELOAD_BYTES);
    if written.is_none_or(|w| sha256_hex(&w) != p.sha256) {
        return Err(SimError::Config(format!(
            "{} does not match the SHA-256 recorded when aispice built it; it was not loaded",
            path.display()
        )));
    }
    Ok(path)
}

/// The environment of a confined run in `run`: these variables and nothing
/// else. `LD_PRELOAD` names the library [`place_preload`] put there. `PWD`
/// is the run folder, as bubblewrap sets it when it enters the folder, so
/// the environment is the same in every sandbox.
pub fn environment(run: &Path) -> Vec<(OsString, OsString)> {
    vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), run.into()),
        ("PWD".into(), run.into()),
        ("TMPDIR".into(), run.into()),
        ("LC_ALL".into(), "C".into()),
        ("LD_PRELOAD".into(), run.join(PRELOAD_FILE).into()),
    ]
}

/// What ngspice at `exe` may touch while it runs in the run folder `run`
/// with `args`: `reads` are the files the deck includes and names and the
/// project's PDK folder ([`super::SimJob::reads`]). A run folder whose path has a space or a colon is refused, since
/// `LD_PRELOAD` splits at both.
pub(crate) fn plan(
    exe: &Path,
    run: &Path,
    reads: &[PathBuf],
    args: Vec<OsString>,
) -> Result<Plan, SimError> {
    let real = |p: &Path| {
        p.canonicalize()
            .map_err(|e| SimError::Io(format!("cannot resolve {}: {e}", p.display())))
    };
    let exe = real(exe)?;
    let run = real(run)?;
    if run.to_string_lossy().contains([' ', ':']) {
        return Err(SimError::Config(format!(
            "the temporary folder {} has a space or a colon in its path, which a sandboxed ngspice cannot preload from; set TMPDIR to a plain path",
            run.display()
        )));
    }
    let mut read = install_dirs(&exe);
    // A folder or file that does not exist has nothing to read.
    read.extend(reads.iter().filter_map(|p| p.canonicalize().ok()));
    read.sort();
    read.dedup();
    Ok(Plan {
        program: exe,
        args,
        env: environment(&run),
        cwd: run.clone(),
        read,
        exec: Vec::new(),
        write: vec![run],
        profile: Profile::NativeCode,
        limits: Limits {
            file_size: Some(MAX_FILE),
            address_space: Some(MAX_MEMORY),
            nproc_headroom: Some(nproc_headroom()),
        },
    })
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
mod tests;
