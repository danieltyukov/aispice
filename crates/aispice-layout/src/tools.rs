//! Finding and running KLayout and Magic.
//!
//! Each run gets a private temporary folder that is also its `HOME`,
//! `KLAYOUT_HOME` and `TMPDIR`, so neither program reads the user's or a
//! project's start-up files or shares temporary files with another run, and
//! runs from an argument vector with a timeout and
//! cancellation. Scripts are aispice's own files, written into the folder;
//! values reach them as `-rd name=value` arguments (KLayout) or environment
//! variables (Magic), never as script text.
//!
//! Both programs read layouts aispice did not write, so they run in the
//! sandbox aispice uses for any program ([`aispice_sim::sandbox`]), under the
//! `Denylist` profile, as defense in depth: each run may read the system,
//! the program's install and what its [`Access`] lists, and write only its
//! private folder and the folders its [`Access`] lists. They run no code from
//! a project (only aispice's scripts and the PDK's decks), so where this
//! machine has no sandbox they still run, and [`unsandboxed`] says why for
//! the tools to tell the user.

use aispice_sim::backend::process::{self, ProcessOutput, ProcessSpec};
use aispice_sim::backend::which;
use aispice_sim::pdk::Pdk;
use aispice_sim::sandbox::{Limits, Plan, Profile, Sandbox};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
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

/// A private folder for one run of a layout program. aispice writes its
/// scripts there before the run and never after: a folder serves one run
/// only ([`klayout`] and [`magic`] refuse a used one), so aispice never
/// writes into a folder a run could have written.
pub struct RunDir {
    _dir: tempfile::TempDir,
    /// Its real path on Linux, where a sandbox grants it by that path.
    path: PathBuf,
    used: std::sync::atomic::AtomicBool,
}

impl RunDir {
    pub fn new() -> Result<Self, LayoutError> {
        let dir = tempfile::Builder::new()
            .prefix("aispice-layout-")
            .tempdir()
            .map_err(|e| LayoutError::Io(format!("cannot create a run folder: {e}")))?;
        let path = if cfg!(target_os = "linux") {
            dir.path()
                .canonicalize()
                .map_err(|e| LayoutError::Io(format!("cannot resolve the run folder: {e}")))?
        } else {
            dir.path().to_path_buf()
        };
        Ok(Self {
            _dir: dir,
            path,
            used: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Mark the folder used by a run; an error when a run had it already.
    fn claim(&self) -> Result<(), LayoutError> {
        if self.used.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return Err(LayoutError::Io(format!(
                "{} served a run already; each run gets a fresh folder",
                self.path.display()
            )));
        }
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write one of aispice's scripts into the folder, before its run.
    pub fn script(&self, name: &str, text: &str) -> Result<PathBuf, LayoutError> {
        self.file(name, text.as_bytes())
    }

    /// Write one of aispice's files into the folder, before its run, as a
    /// new file ([`create_file`]).
    pub fn file(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, LayoutError> {
        if self.used.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(LayoutError::Io(format!(
                "{} served a run already; aispice writes nothing into it after",
                self.path.display()
            )));
        }
        let p = self.path().join(name);
        create_file(&p, bytes)?;
        Ok(p)
    }
}

/// Create `path` holding `bytes`, refusing whatever is there already: a
/// file, a link (never followed) or a fifo (`O_CREAT|O_EXCL|O_NOFOLLOW`).
/// For aispice's own files beside a run's.
pub fn create_file(path: &Path, bytes: &[u8]) -> Result<(), LayoutError> {
    use std::io::Write as _;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = o
        .open(path)
        .map_err(|e| LayoutError::Io(format!("{}: {e}", path.display())))?;
    f.write_all(bytes)
        .map_err(|e| LayoutError::Io(format!("{}: {e}", path.display())))
}

/// Whether a run left a plain file at `path`; a link is not one.
pub fn is_output(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

/// A file a layout run wrote: opened without following a link and without
/// blocking on a fifo, and read only when it is a plain file of at most
/// `limit` bytes. A run could have left anything at that name.
pub fn read_output(path: &Path, limit: u64) -> Result<Vec<u8>, LayoutError> {
    use std::io::Read as _;
    let io = |e: std::io::Error| LayoutError::Io(format!("{}: {e}", path.display()));
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(path).is_ok_and(|m| !m.file_type().is_file()) {
        return Err(LayoutError::Io(format!(
            "{} is not a plain file",
            path.display()
        )));
    }
    let f = o.open(path).map_err(io)?;
    let meta = f.metadata().map_err(io)?;
    if !meta.is_file() {
        return Err(LayoutError::Io(format!(
            "{} is not a plain file",
            path.display()
        )));
    }
    let too_large = || {
        LayoutError::Input(format!(
            "{} is larger than {} MiB",
            path.display(),
            limit >> 20
        ))
    };
    if meta.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    f.take(limit + 1).read_to_end(&mut bytes).map_err(io)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(bytes)
}

/// [`read_output`] of a text file.
pub fn read_output_text(path: &Path, limit: u64) -> Result<String, LayoutError> {
    String::from_utf8(read_output(path, limit)?)
        .map_err(|_| LayoutError::Input(format!("{} is not text", path.display())))
}

/// How long a layout program may run.
pub const TIMEOUT: Duration = Duration::from_secs(1800);
/// The largest file a layout program may write (`RLIMIT_FSIZE`).
const MAX_FILE: u64 = 8 << 30;
/// Processes and threads a layout program may add to what the user runs
/// (`RLIMIT_NPROC`, relative): KLayout's DRC runs threads.
const NPROC_HEADROOM: u64 = 512;
/// Variables a run keeps from aispice's environment, besides the ones each
/// run sets: where programs and libraries are, the locale, and what a
/// Windows program needs to start. Nothing else of aispice's reaches it, so
/// neither `KLAYOUT_PATH` nor `PYTHONPATH` (macros and modules a user or a
/// project set up) does.
const PASSED_ENV: &[&str] = &[
    "PATH",
    "LD_LIBRARY_PATH",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "PATHEXT",
    "COMSPEC",
];
/// System folders a layout program reads besides those every sandboxed
/// program may: fonts and their configuration, for Qt's offscreen platform.
const LAYOUT_SYSTEM: &[&str] = &["/etc/fonts"];

/// What one layout run may touch besides the system, the program's install
/// and its private folder (which it may write): the files and folders it
/// reads, such as the PDK and its input layouts, and the folders it writes
/// its results into. Each path must be the real path of what it names
/// ([`resolved`]) for a sandboxed run.
#[derive(Debug, Clone, Default)]
pub struct Access {
    read: Vec<PathBuf>,
    write: Vec<PathBuf>,
}

impl Access {
    pub fn new() -> Self {
        Self::default()
    }

    /// Let the run read `p`, a file or a folder.
    pub fn read(mut self, p: impl AsRef<Path>) -> Self {
        self.read.push(p.as_ref().to_path_buf());
        self
    }

    /// Let the run write in the folder `p`, which must exist.
    pub fn write(mut self, p: impl AsRef<Path>) -> Self {
        self.write.push(p.as_ref().to_path_buf());
        self
    }

    /// Let the run write the file `p`: in its folder.
    pub fn write_file(self, p: &Path) -> Self {
        match p.parent() {
            Some(dir) => self.write(dir),
            None => self,
        }
    }
}

/// `p` itself, when it is the real path of what it names: absolute, with no
/// `.` or `..`, no trailing separator and no link in any component, so it
/// equals its real path byte for byte. A grant is given by real path, and
/// the program opens the path as written; were the two different, a grant
/// through a link would cover what the link points at, and `run/../x`
/// would read as inside `run`. Such a path is refused, never mended.
fn resolved(p: &Path) -> Result<PathBuf, String> {
    if !p.is_absolute() {
        return Err(format!("{} is not an absolute path", p.display()));
    }
    let written = p.to_string_lossy();
    if written.split(['/', '\\']).any(|c| c == "." || c == "..") {
        return Err(format!(
            "{} has a `.` or `..` in it; a layout run takes real paths only",
            p.display()
        ));
    }
    let real = p
        .canonicalize()
        .map_err(|e| format!("{}: {e}", p.display()))?;
    if real.as_os_str() != p.as_os_str() {
        return Err(format!(
            "{} is not the real path of what it names ({}); a layout run takes real paths only",
            p.display(),
            real.display()
        ));
    }
    Ok(real)
}

/// `p` with links resolved, so a rule or bind names the file itself.
fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The sandbox layout programs run in here, chosen once per process, or
/// why there is none.
async fn sandbox() -> Result<&'static Sandbox, &'static str> {
    static CHOSEN: tokio::sync::OnceCell<Result<Sandbox, String>> =
        tokio::sync::OnceCell::const_new();
    // Not the caller's token: a cancelled run must not leave every later run
    // without a sandbox.
    CHOSEN
        .get_or_init(|| async { Sandbox::select(&CancellationToken::new()).await })
        .await
        .as_ref()
        .map_err(String::as_str)
}

/// Why KLayout and Magic run without a sandbox on this machine, if they do,
/// as a note for the tools to show.
pub async fn unsandboxed() -> Option<String> {
    sandbox().await.err().map(|why| {
        format!(
            "KLayout and Magic ran without a sandbox here ({why}). They run no code from the project, so they ran anyway; see docs/SANDBOX.md to enable one."
        )
    })
}

/// The sandbox KLayout and Magic run in here, or `none` and why, for
/// `aispice doctor`.
pub async fn sandbox_description() -> String {
    match sandbox().await {
        Ok(s) => s.describe(),
        Err(why) => format!("none ({why}); they run unconfined"),
    }
}

/// One run of a layout program, before the sandbox is chosen.
struct Run<'a> {
    name: &'static str,
    program: PathBuf,
    args: Vec<OsString>,
    /// Variables the run sets; the rest of a sandboxed run's environment
    /// is [`PASSED_ENV`].
    env: Vec<(OsString, OsString)>,
    dir: &'a Path,
    access: &'a Access,
    /// Programs it may start besides itself (a start-up script's
    /// interpreter and helpers).
    exec: Vec<PathBuf>,
    timeout: Duration,
}

/// Run `run` in the sandbox when this machine has one. Without one it runs
/// unconfined, but with the same private folder as its working folder and
/// home and the same cleared environment.
async fn run(run: Run<'_>, cancel: &CancellationToken) -> Result<ProcessOutput, LayoutError> {
    let failed = |message: String| LayoutError::Failed {
        program: run.name.into(),
        message,
    };
    let spec = match sandbox().await {
        Ok(sandbox) => {
            let plan = plan(&run).map_err(LayoutError::Input)?;
            sandbox.command(&plan).map_err(|e| failed(e.to_string()))?
        }
        Err(why) => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| tracing::warn!("KLayout and Magic run without a sandbox: {why}"));
            unconfined(&plan_unchecked(&run))
        }
    };
    process::run(&spec, run.timeout, cancel)
        .await
        .map_err(|e| failed(e.to_string()))
}

/// The command for `plan` where there is no sandbox: its program and
/// arguments in its working folder, with exactly its environment.
fn unconfined(plan: &Plan) -> ProcessSpec {
    let mut spec = ProcessSpec::new(&plan.program, &plan.cwd);
    spec.args = plan.args.clone();
    spec.env = plan.env.clone();
    spec.clear_env = true;
    spec
}

/// The sandbox plan of a run: the `Denylist` profile, the program's
/// install, fonts and the run's [`Access`] to read, its private folder and
/// the run's folders to write. Its folder and every path of its access must
/// be real paths ([`resolved`]).
fn plan(run: &Run<'_>) -> Result<Plan, String> {
    resolved(run.dir)?;
    for p in run.access.read.iter().chain(&run.access.write) {
        resolved(p)?;
    }
    Ok(plan_unchecked(run))
}

/// [`plan`] without checking the paths, for a run without a sandbox, which
/// grants nothing.
fn plan_unchecked(run: &Run<'_>) -> Plan {
    let mut env: Vec<(OsString, OsString)> = PASSED_ENV
        .iter()
        .filter_map(|k| std::env::var_os(k).map(|v| (OsString::from(k), v)))
        .collect();
    for (k, v) in &run.env {
        env.retain(|(x, _)| x != k);
        env.push((k.clone(), v.clone()));
    }
    let mut read: Vec<PathBuf> = LAYOUT_SYSTEM
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    read.extend(install_folders(&run.program, home.as_deref()));
    read.extend(run.access.read.iter().cloned());
    let dir = run.dir.to_path_buf();
    let mut write = vec![dir.clone()];
    write.extend(run.access.write.iter().cloned());
    Plan {
        program: run.program.clone(),
        args: run.args.clone(),
        env,
        cwd: dir,
        read,
        exec: run.exec.clone(),
        write,
        profile: Profile::Denylist,
        limits: Limits {
            file_size: Some(MAX_FILE),
            address_space: None,
            nproc_headroom: Some(NPROC_HEADROOM),
        },
    }
}

/// What of a program installed outside `/usr` (which every sandboxed
/// program reads) a run may read: the program file itself, and where it
/// sits in a real install, `<prefix>/bin` with `lib`, `lib64` or `share`
/// beside it, that install's `bin`, `lib`, `lib64` and `share`. A program
/// anywhere else (`~/Downloads`, `~/bin`) brings only its own file. Never
/// `/`, the home folder or a folder above it as a prefix; and the home's
/// `.local`, which other programs share, brings only the program's own
/// `lib/<name>` and `share/<name>`, not its `bin`.
fn install_folders(program: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    // Both sides of every comparison are real paths: a home reached
    // through a link is still the home.
    let home = home.map(real);
    let home = home.as_deref();
    let real = real(program);
    if real.starts_with("/usr") {
        return Vec::new();
    }
    let mut v = vec![real.clone()];
    let Some(bin) = real
        .parent()
        .filter(|b| b.file_name() == Some(std::ffi::OsStr::new("bin")))
    else {
        return v;
    };
    let holds_home = |p: &Path| p.parent().is_none() || home.is_some_and(|h| h.starts_with(p));
    let Some(prefix) = bin.parent().filter(|p| !holds_home(p)) else {
        return v;
    };
    let shared = home.is_some_and(|h| prefix == h.join(".local"));
    let name = real.file_name().unwrap_or_default();
    let beside: Vec<PathBuf> = ["lib", "lib64", "share"]
        .iter()
        .map(|sub| {
            if shared {
                prefix.join(sub).join(name)
            } else {
                prefix.join(sub)
            }
        })
        .filter(|p| p.is_dir())
        .collect();
    if beside.is_empty() {
        return v;
    }
    if !shared {
        v.push(bin.to_path_buf());
    }
    v.extend(beside);
    v
}

/// What Magic's start-up script may run: Magic is usually a shell script
/// (`#!/usr/bin/env bash`) that starts `magicdnull` or `magicexec` from its
/// library folder, after a `uname | cut` for Cygwin.
fn magic_helpers(program: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = ["env", "bash", "sh", "uname", "cut"]
        .iter()
        .filter_map(|h| which(h))
        .collect();
    let real = real(program);
    if let Some(prefix) = real.parent().and_then(Path::parent) {
        let mut libs = vec![prefix.join("lib"), prefix.join("lib64")];
        if let Ok(entries) = std::fs::read_dir(prefix.join("lib")) {
            libs.extend(entries.flatten().map(|e| e.path()));
        }
        v.extend(
            libs.into_iter()
                .map(|l| l.join("magic/tcl"))
                .filter(|p| p.is_dir()),
        );
    }
    v
}

/// Run `klayout -b -r <script> -rd k=v ...` in the fresh folder `run`,
/// with `access`.
pub async fn klayout(
    programs: &Programs,
    run: &RunDir,
    script: &Path,
    vars: &[(&str, String)],
    access: &Access,
    cancel: &CancellationToken,
) -> Result<ProcessOutput, LayoutError> {
    let program = programs.klayout()?;
    let dir = run.path();
    let python = no_psutil(dir)?;
    run.claim()?;
    let mut args: Vec<OsString> = vec!["-b".into(), "-r".into(), script.into()];
    for (k, v) in vars {
        args.push("-rd".into());
        args.push(format!("{k}={v}").into());
    }
    let env = vec![
        ("HOME".into(), dir.into()),
        ("KLAYOUT_HOME".into(), dir.into()),
        // IHP's pcell library turns its `#ifdef KLAYOUT` blocks on when a
        // process up its chain is named klayout, which it asks psutil, or
        // when a variable of that name is set. psutil reads the `/proc` of
        // every ancestor, which the sandbox does not show, and the library
        // does not catch the refusal; without psutil it falls back to the
        // variable. KLayout's Python reads KLAYOUT_PYTHONPATH, ahead of the
        // system's folders, in place of PYTHONPATH.
        ("KLAYOUT".into(), "1".into()),
        ("KLAYOUT_PYTHONPATH".into(), python.into()),
        // IHP's pcell library writes preprocessed modules under fixed names
        // in the temporary folder and deletes them after loading: a shared
        // /tmp lets two runs delete each other's files, or a local user
        // plant one. Each run gets its own.
        ("TMPDIR".into(), dir.into()),
        ("TMP".into(), dir.into()),
        ("TEMP".into(), dir.into()),
        // Batch mode opens no window; this keeps Qt off the display too.
        ("QT_QPA_PLATFORM".into(), "offscreen".into()),
    ];
    let access = access.clone().read(script);
    self::run(
        Run {
            name: "KLayout",
            program,
            args,
            env,
            dir,
            access: &access,
            exec: Vec::new(),
            timeout: TIMEOUT,
        },
        cancel,
    )
    .await
}

/// A folder in `dir` holding a `psutil` that cannot be imported, for
/// KLayout's Python to find first.
fn no_psutil(dir: &Path) -> Result<PathBuf, LayoutError> {
    let io = |e: std::io::Error| LayoutError::Io(format!("cannot prepare KLayout's Python: {e}"));
    let python = dir.join("aispice-python");
    let module = python.join("psutil");
    // New folders in the run's fresh folder, before the run.
    std::fs::create_dir(&python).map_err(io)?;
    std::fs::create_dir(&module).map_err(io)?;
    create_file(
        &module.join("__init__.py"),
        b"raise ImportError('psutil is not available to KLayout under aispice')\n",
    )?;
    Ok(python)
}

/// Run Magic without graphics on a Tcl script of aispice's in the fresh
/// folder `run`, with the PDK's start-up file, the given environment and
/// `access`.
#[allow(clippy::too_many_arguments)]
pub async fn magic(
    programs: &Programs,
    run: &RunDir,
    rcfile: &Path,
    script: &Path,
    env: &[(&str, String)],
    access: &Access,
    cancel: &CancellationToken,
) -> Result<ProcessOutput, LayoutError> {
    let program = programs.magic()?;
    let dir = run.path();
    run.claim()?;
    let args: Vec<OsString> = vec![
        "-dnull".into(),
        "-noconsole".into(),
        "-rcfile".into(),
        rcfile.into(),
        script.into(),
    ];
    let mut vars: Vec<(OsString, OsString)> =
        vec![("HOME".into(), dir.into()), ("TMPDIR".into(), dir.into())];
    vars.extend(
        env.iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
    );
    let access = access.clone().read(rcfile).read(script);
    let exec = magic_helpers(&program);
    self::run(
        Run {
            name: "Magic",
            program,
            args,
            env: vars,
            dir,
            access: &access,
            exec,
            timeout: TIMEOUT,
        },
        cancel,
    )
    .await
}

/// The version a layout program reports (`klayout -v`, `magic --version`),
/// for evidence summaries; `unknown` when it does not say.
pub async fn version(programs: &Programs, magic: bool, cancel: &CancellationToken) -> String {
    let (exe, flag) = if magic {
        (programs.magic(), "--version")
    } else {
        (programs.klayout(), "-v")
    };
    let (Ok(program), Ok(dir)) = (exe, RunDir::new()) else {
        return "unknown".into();
    };
    let exec = if magic {
        magic_helpers(&program)
    } else {
        Vec::new()
    };
    let r = Run {
        name: if magic { "Magic" } else { "KLayout" },
        program,
        args: vec![flag.into()],
        env: vec![
            ("HOME".into(), dir.path().into()),
            ("KLAYOUT_HOME".into(), dir.path().into()),
            ("QT_QPA_PLATFORM".into(), "offscreen".into()),
        ],
        dir: dir.path(),
        access: &Access::new(),
        exec,
        timeout: Duration::from_secs(60),
    };
    match run(r, cancel).await {
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

    /// A run's plan: the Denylist profile, the run's own reads and writes
    /// on top of its private folder, its variables over the few kept from
    /// aispice's environment, and nothing else of that environment.
    #[test]
    fn a_layout_run_is_planned_with_its_access_only() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let (input, out) = (dir.join("in.gds"), dir.join("out"));
        std::fs::write(&input, "").unwrap();
        std::fs::create_dir(&out).unwrap();
        let access = Access::new()
            .read(&input)
            .write_file(&out.join("report.lyrdb"));
        let run = Run {
            name: "KLayout",
            program: PathBuf::from("/usr/bin/klayout"),
            args: vec!["-b".into()],
            env: vec![
                ("HOME".into(), dir.clone().into()),
                ("LANG".into(), "C".into()),
            ],
            dir: &dir,
            access: &access,
            exec: Vec::new(),
            timeout: TIMEOUT,
        };
        let p = plan(&run).unwrap();
        assert_eq!(p.profile, Profile::Denylist);
        assert_eq!(p.cwd, dir);
        assert_eq!(p.write, vec![dir.clone(), out.clone()]);
        assert!(p.read.contains(&input));
        assert!(p.exec.is_empty());
        assert_eq!(p.limits.file_size, Some(MAX_FILE));
        for (k, _) in &p.env {
            let k = k.to_string_lossy();
            assert!(k == "HOME" || PASSED_ENV.contains(&k.as_ref()), "{k}");
        }
        // A variable the run sets replaces the one kept from aispice's.
        let langs: Vec<_> = p.env.iter().filter(|(k, _)| k == "LANG").collect();
        assert_eq!(langs.len(), 1);
        assert_eq!(langs[0].1, "C");
    }

    /// A program outside /usr brings its own file, and from a real install
    /// (`<prefix>/bin` beside `lib`, `lib64` or `share`) that install's
    /// folders; never a folder holding the home, nothing from a folder that
    /// is not an install, and from the home's shared `.local` only the
    /// program's own subfolders.
    #[test]
    fn install_folders_stop_short_of_home() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let prefix = root.join("opt");
        for d in ["bin", "lib", "share"] {
            std::fs::create_dir_all(prefix.join(d)).unwrap();
        }
        let program = prefix.join("bin/klayout");
        std::fs::write(&program, "").unwrap();
        assert_eq!(
            install_folders(&program, None),
            [
                program.clone(),
                prefix.join("bin"),
                prefix.join("lib"),
                prefix.join("share")
            ]
        );
        // The install is the home, or the home is below the program.
        assert_eq!(
            install_folders(&program, Some(&prefix)),
            std::slice::from_ref(&program)
        );
        assert_eq!(
            install_folders(&program, Some(&prefix.join("bin/x"))),
            std::slice::from_ref(&program)
        );
        // A program outside a bin folder, or in a bin folder with nothing
        // beside it, brings only its own file.
        let downloads = root.join("Downloads");
        std::fs::create_dir_all(&downloads).unwrap();
        std::fs::write(downloads.join("klayout"), "").unwrap();
        assert_eq!(
            install_folders(&downloads.join("klayout"), None),
            [downloads.join("klayout")]
        );
        let bare = root.join("bare/bin");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(bare.join("magic"), "").unwrap();
        assert_eq!(
            install_folders(&bare.join("magic"), None),
            [bare.join("magic")]
        );
        // ~/.local: its own lib and share folders only, not ~/.local/bin.
        let home = root.join("home");
        let local = home.join(".local");
        for d in ["bin", "lib/klayout", "share/klayout", "share/keyrings"] {
            std::fs::create_dir_all(local.join(d)).unwrap();
        }
        let program = local.join("bin/klayout");
        std::fs::write(&program, "").unwrap();
        assert_eq!(
            install_folders(&program, Some(&home)),
            [
                program.clone(),
                local.join("lib/klayout"),
                local.join("share/klayout")
            ]
        );
        // ~/bin: the home is the prefix, so only the file.
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("bin/klayout"), "").unwrap();
        assert_eq!(
            install_folders(&home.join("bin/klayout"), Some(&home)),
            [home.join("bin/klayout")]
        );
        assert!(install_folders(Path::new("/usr/bin/klayout"), None).is_empty());
        // A home reached through a link is the home all the same.
        #[cfg(unix)]
        {
            let link = root.join("home-link");
            std::os::unix::fs::symlink(&prefix, &link).unwrap();
            let program = prefix.join("bin/klayout");
            assert_eq!(
                install_folders(&program, Some(&link)),
                std::slice::from_ref(&program)
            );
        }
    }

    /// A link a run left in its folder is never followed: not when aispice
    /// reads what the run wrote, not when it writes a file of its own there,
    /// and a used run folder takes no more of aispice's files.
    #[cfg(unix)]
    #[test]
    fn planted_links_are_refused_on_read_and_on_write() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let victim = root.join("bashrc");
        std::fs::write(&victim, "SECRET").unwrap();
        let out = root.join("run");
        std::fs::create_dir(&out).unwrap();
        let planted = out.join("chip.lvs.spice");
        std::os::unix::fs::symlink(&victim, &planted).unwrap();
        // Read: refused, and not read through.
        assert!(!is_output(&planted));
        let err = read_output(&planted, 1 << 20).unwrap_err().to_string();
        assert!(!err.contains("SECRET"), "{err}");
        assert!(read_output_text(&planted, 1 << 20).is_err());
        // Write: refused, and the target is untouched.
        assert!(create_file(&planted, b"overwritten").is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "SECRET");
        // A dangling link is refused too.
        let dangling = out.join("layout.png");
        std::os::unix::fs::symlink(root.join("nowhere"), &dangling).unwrap();
        assert!(create_file(&dangling, b"x").is_err());
        assert!(!root.join("nowhere").exists());
        // A fifo is refused without blocking.
        let fifo = out.join("report.lyrdb");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        assert!(read_output(&fifo, 1 << 20).is_err());
        // Plain files work, within their limit.
        let plain = out.join("plain.txt");
        create_file(&plain, b"hello").unwrap();
        assert!(is_output(&plain));
        assert_eq!(read_output_text(&plain, 5).unwrap(), "hello");
        assert!(read_output(&plain, 4).is_err());
        assert!(create_file(&plain, b"again").is_err());
        // A run folder serves one run, and takes no files after it.
        let run = RunDir::new().unwrap();
        run.script("a.py", "").unwrap();
        run.claim().unwrap();
        assert!(run.claim().is_err());
        assert!(run.script("b.py", "").is_err());
    }

    /// A sandboxed run takes real paths only: a `..`, a `.`, a trailing
    /// separator or a link in an input path or in an output folder is
    /// refused, since the grant (by real path) and what the program opens
    /// (the path as written) would differ.
    #[cfg(unix)]
    #[test]
    fn a_sandboxed_run_takes_real_paths_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("project");
        let run_folder = project.join(".aispice/layout/run-1");
        let outside = root.join("outside");
        for d in [&run_folder, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        let input = project.join("in.gds");
        std::fs::write(&input, "").unwrap();
        std::os::unix::fs::symlink(&outside, project.join("link")).unwrap();
        std::fs::write(outside.join("in.gds"), "").unwrap();
        let dir = root.join("private");
        std::fs::create_dir(&dir).unwrap();
        let plan_with = |access: Access| {
            let run = Run {
                name: "KLayout",
                program: PathBuf::from("/usr/bin/klayout"),
                args: Vec::new(),
                env: Vec::new(),
                dir: &dir,
                access: &access,
                exec: Vec::new(),
                timeout: TIMEOUT,
            };
            plan(&run).map(|_| ())
        };
        assert!(plan_with(Access::new().read(&input).write(&run_folder)).is_ok());
        let up = |p: &Path, rest: &str| PathBuf::from(format!("{}/{rest}", p.display()));
        for (what, access) in [
            (
                "input with ..",
                Access::new().read(up(&run_folder, "../../../in.gds")),
            ),
            ("input with .", Access::new().read(up(&project, "./in.gds"))),
            (
                "input through a link",
                Access::new().read(project.join("link/in.gds")),
            ),
            ("relative input", Access::new().read("in.gds")),
            (
                "output with ..",
                Access::new().write(up(&run_folder, "../../..")),
            ),
            (
                "output through a link",
                Access::new().write(project.join("link")),
            ),
            (
                "output with a trailing separator",
                Access::new().write(up(&run_folder, "")),
            ),
        ] {
            assert!(plan_with(access).is_err(), "{what}");
        }
        // The run's own folder too.
        let run = Run {
            name: "KLayout",
            program: PathBuf::from("/usr/bin/klayout"),
            args: Vec::new(),
            env: Vec::new(),
            dir: &up(&dir, "."),
            access: &Access::new(),
            exec: Vec::new(),
            timeout: TIMEOUT,
        };
        assert!(plan(&run).is_err());
    }

    /// Without a sandbox a run keeps its private working folder and exactly
    /// its environment, none of aispice's.
    #[test]
    fn an_unconfined_run_keeps_its_folder_and_environment() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let access = Access::new();
        let run = Run {
            name: "KLayout",
            program: PathBuf::from("/usr/bin/klayout"),
            args: vec!["-b".into()],
            env: vec![("HOME".into(), dir.clone().into())],
            dir: &dir,
            access: &access,
            exec: Vec::new(),
            timeout: TIMEOUT,
        };
        let p = plan_unchecked(&run);
        let spec = unconfined(&p);
        assert!(spec.clear_env);
        assert_eq!(spec.cwd, dir);
        assert_eq!(spec.env, p.env);
        assert!(spec.confine.is_none());
        assert!(
            spec.env
                .iter()
                .any(|(k, v)| k == "HOME" && Path::new(v) == dir)
        );
    }

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
