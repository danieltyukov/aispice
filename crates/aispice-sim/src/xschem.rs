//! Running xschem as a netlister on a schematic aispice wrote itself.
//!
//! xschem evaluates Tcl from the files it opens: property values, symbol
//! formats and texts, symbols it loads by library path, and the rc files it
//! sources. So aispice never gives it a file it did not write: [`netlist`]
//! takes the schematic's text, not a path, and writes it into a fresh
//! private folder itself. The only caller in the product is the check of an
//! export, whose text comes from the export writer that keeps every value
//! literal (`aispice_core::xschem::export`); an imported schematic is never
//! run through xschem.
//!
//! The run is sandboxed as defense in depth, in this order
//! ([`Sandbox::select`]):
//!
//! - bubblewrap (`bwrap`), when it can make its namespaces on this machine:
//!   new user, PID, IPC, UTS and network namespaces (no network at all), a
//!   fresh root on which only these are visible: the system folders (`/usr`
//!   and the links into it) read-only, xschem itself and its shared folder
//!   read-only, the library and model folders and the private input folder
//!   read-only, the rc file read-only, a private `/tmp` (and the home inside
//!   it) on a small tmpfs, and one writable folder for the netlist;
//! - else Landlock at ABI 6 or later with a seccomp filter
//!   (`crate::backend::confine`). This is the path on a machine whose kernel
//!   restricts unprivileged user namespaces without a bwrap AppArmor
//!   profile (Ubuntu 24.04's default);
//! - when neither works, xschem is not run, and the caller reports the file
//!   as not verified with xschem.
//!
//! Either way the process gets a file size and address space limit, dies
//! with aispice, starts in its own process group, and is run:
//!
//! - from an argument vector, with stdin closed, a timeout and capped
//!   output, headless (`-x`: no window, `-q`: quit when done), in the
//!   output folder;
//! - with an environment of `PATH`, `HOME`, `TMPDIR`, `LC_ALL` and the
//!   variables the rc file reads, nothing inherited;
//! - with `--rcfile` pointing at a fixed rc aispice writes, which reads the
//!   library path, the output folder and the model folders from environment
//!   variables, so no path is ever spliced into Tcl text. Given `--rcfile`,
//!   xschem sources no other rc file: not `~/.xschem/xschemrc`, not an
//!   `xschemrc` in the working folder;
//! - only when every path xschem sees consists of letters, digits and
//!   `/ . _ - + , = @` (xschem splices file names into Tcl commands).
//!
//! The netlist it writes is read back through one descriptor opened without
//! following a symlink, so nothing left behind in the writable folder can
//! point the read elsewhere.

use crate::backend::process::{self, Confinement, Landlock, ProcessSpec};
use crate::backend::{SimError, which};
use aispice_core::xschem::safety::safe_path;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A netlist run takes about a tenth of a second; this is generous.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Netlists larger than this are not read.
const MAX_NETLIST: u64 = 16 << 20;
/// The sandbox's `/tmp` (and the home inside it) holds at most this much.
const TMP_BYTES: u64 = 64 << 20;
/// No file xschem writes may grow past this (`RLIMIT_FSIZE`).
const MAX_FILE: u64 = 64 << 20;
/// xschem's address space (`RLIMIT_AS`).
const MAX_MEMORY: u64 = 4 << 30;

/// Where things appear inside the bubblewrap namespace. None of these exist
/// outside it; the Landlock path uses the real host folders instead.
pub const SANDBOX_OUT: &str = "/tmp/aispice/out";
pub const SANDBOX_RC: &str = "/tmp/aispice/xschemrc.tcl";
pub const SANDBOX_HOME: &str = "/tmp/aispice/home";

/// The rc file every run uses. It never contains a path: those come from
/// the environment.
pub const RC: &str = "\
# Written by aispice for one headless netlist run.
set XSCHEM_LIBRARY_PATH $env(AISPICE_XSCHEM_LIBRARY_PATH)
set netlist_dir $env(AISPICE_XSCHEM_NETLIST_DIR)
# xschem otherwise makes undo and temp folders under a fixed system temp
# path the sandbox does not let it write; keep them in the private TMPDIR.
set XSCHEM_TMP_DIR $env(TMPDIR)
if {[info exists env(AISPICE_MODELS_NGSPICE)]} {
  set MODELS_NGSPICE $env(AISPICE_MODELS_NGSPICE)
  set SG13G2_MODELS $env(AISPICE_MODELS_NGSPICE)
}
if {[info exists env(AISPICE_MODELS_XYCE)]} {
  set MODELS_XYCE $env(AISPICE_MODELS_XYCE)
  set SG13G2_MODELS_XYCE $env(AISPICE_MODELS_XYCE)
}
";

/// Why xschem is not run when no sandbox is available on this machine.
pub const NO_SANDBOX: &str = "aispice runs xschem only inside a sandbox, and this machine has neither: bubblewrap (bwrap) could not start (install the bubblewrap package, or on Ubuntu 24.04 add its AppArmor profile; see docs/XSCHEM.md), and the kernel has no Landlock support";

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

/// The way xschem is confined on this machine, chosen by [`Sandbox::select`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sandbox {
    /// bubblewrap, with its own namespaces. The `PathBuf` is `bwrap`.
    Bwrap(PathBuf),
    /// Landlock, applied in a pre_exec hook (Linux only). `abi` is the
    /// kernel's Landlock ABI, kept only for messages and tests.
    Landlock { abi: i32 },
}

impl Sandbox {
    /// The sandbox to use: bubblewrap when it can make its namespaces here,
    /// else Landlock when the kernel has it (ABI
    /// [`confine::MIN_ABI`](crate::backend::confine::MIN_ABI) or later), else
    /// the reason neither is available. Runs `bwrap` once to see whether
    /// this machine lets it, since a restricted kernel refuses it even when
    /// it is installed.
    pub async fn select(cancel: &CancellationToken) -> Result<Self, String> {
        let mut why = Vec::new();
        match which("bwrap") {
            Some(bwrap) => match probe_bwrap(&bwrap, cancel).await {
                Ok(()) => return Ok(Sandbox::Bwrap(bwrap)),
                Err(e) => why.push(format!("bubblewrap could not start a sandbox: {e}")),
            },
            None => why.push("bubblewrap (bwrap) is not installed".into()),
        }
        match landlock_choice() {
            Ok(abi) => return Ok(Sandbox::Landlock { abi }),
            Err(e) => why.push(e),
        }
        Err(format!(
            "aispice runs xschem only inside a sandbox, and this machine has neither: {} (see docs/XSCHEM.md)",
            why.join("; ")
        ))
    }
}

/// The kernel's Landlock ABI if it is usable (at least
/// [`confine::MIN_ABI`](crate::backend::confine::MIN_ABI), which has the
/// network rules), else why it is not. Off Linux there is no Landlock.
#[cfg(target_os = "linux")]
fn landlock_choice() -> Result<i32, String> {
    landlock_choice_from(crate::backend::confine::abi())
}

/// Whether a given Landlock ABI is usable. Separate from the kernel probe so
/// a test can check the floor without the kernel's real value.
#[cfg(target_os = "linux")]
fn landlock_choice_from(abi: i32) -> Result<i32, String> {
    let min = crate::backend::confine::MIN_ABI;
    match abi {
        a if a >= min => Ok(a),
        0 => Err("the kernel has no Landlock support".into()),
        a => Err(format!(
            "the kernel's Landlock ABI {a} is below {min}, which it needs to scope sockets and signals"
        )),
    }
}
#[cfg(not(target_os = "linux"))]
fn landlock_choice() -> Result<i32, String> {
    Err("this platform has no Landlock".into())
}

/// Start an empty bubblewrap sandbox and run `true` in it: whether this
/// machine lets it make its namespaces (some kernels restrict unprivileged
/// user namespaces).
async fn probe_bwrap(bwrap: &Path, cancel: &CancellationToken) -> Result<(), String> {
    let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
    let rc = tmp.path().join("rc");
    std::fs::write(&rc, "").map_err(|e| e.to_string())?;
    let view = View {
        read_only: Vec::new(),
        out: tmp.path().to_path_buf(),
        rc,
    };
    let truth = which("true").unwrap_or_else(|| PathBuf::from("/usr/bin/true"));
    let mut spec = ProcessSpec::new(bwrap, tmp.path());
    spec.args = bwrap_args(&view, &[], &truth, &[]);
    spec.clear_env = true;
    let out = process::run(&spec, Duration::from_secs(10), cancel)
        .await
        .map_err(|e| e.to_string())?;
    if out.success() {
        Ok(())
    } else {
        Err(first_line(&out.combined()).unwrap_or_else(|| "bwrap failed".into()))
    }
}

/// What one bubblewrap run shows besides the system folders.
#[derive(Debug, Clone, Default)]
pub struct View {
    /// Folders and files shown read-only at their own paths.
    pub read_only: Vec<PathBuf>,
    /// The one writable folder, shown at [`SANDBOX_OUT`].
    pub out: PathBuf,
    /// The rc file, shown read-only at [`SANDBOX_RC`].
    pub rc: PathBuf,
}

/// System folders shown read-only: programs, libraries, Tcl. The usual
/// top-level links into `/usr` are recreated as links.
const SYSTEM: &[&str] = &["/usr"];
const SYSTEM_LINKS: &[&str] = &["/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32"];
/// Single system files and folders the loader and `awk` need, when present.
const SYSTEM_TRY: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/alternatives",
];

/// Whether `p` is already visible through a system folder.
fn under_system(p: &Path) -> bool {
    SYSTEM.iter().any(|s| p.starts_with(s))
}

/// The bubblewrap arguments that run `program args` seeing only `view`, the
/// system folders and `env`.
pub fn bwrap_args(
    view: &View,
    env: &[(String, OsString)],
    program: &Path,
    args: &[OsString],
) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |items: &[&dyn AsRef<std::ffi::OsStr>]| {
        for i in items {
            a.push(i.as_ref().to_os_string());
        }
    };
    push(&[
        &"--unshare-all",
        &"--unshare-user",
        &"--die-with-parent",
        &"--new-session",
        &"--clearenv",
    ]);
    for (k, v) in env {
        push(&[&"--setenv", k, v]);
    }
    for s in SYSTEM {
        push(&[&"--ro-bind", s, s]);
    }
    for l in SYSTEM_LINKS {
        let p = Path::new(l);
        match std::fs::read_link(p) {
            Ok(target) => push(&[&"--symlink", &target, l]),
            Err(_) if p.is_dir() => push(&[&"--ro-bind", l, l]),
            Err(_) => {}
        }
    }
    for t in SYSTEM_TRY {
        push(&[&"--ro-bind-try", t, t]);
    }
    let size = TMP_BYTES.to_string();
    push(&[
        &"--proc",
        &"/proc",
        &"--dev",
        &"/dev",
        &"--size",
        &size,
        &"--tmpfs",
        &"/tmp",
        &"--dir",
        &SANDBOX_HOME,
    ]);
    let mut shown: Vec<&PathBuf> = Vec::new();
    for p in &view.read_only {
        if under_system(p) || shown.iter().any(|s| p.starts_with(s)) {
            continue;
        }
        shown.push(p);
        push(&[&"--ro-bind", p, p]);
    }
    push(&[
        &"--ro-bind",
        &view.rc,
        &SANDBOX_RC,
        &"--bind",
        &view.out,
        &SANDBOX_OUT,
        &"--chdir",
        &SANDBOX_OUT,
        &"--",
        &program,
    ]);
    a.extend(args.iter().cloned());
    a
}

/// What one run reads besides the schematic, which aispice writes itself.
#[derive(Debug, Clone, Default)]
pub struct Run {
    /// `XSCHEM_LIBRARY_PATH`, in order: xschem's own library and the
    /// registered PDK's, never a folder a schematic names.
    pub library: Vec<PathBuf>,
    pub models_ngspice: Option<PathBuf>,
    pub models_xyce: Option<PathBuf>,
}

/// The library and model folders: read-only system or PDK roots, the only
/// folders besides the system that the sandboxed process may read.
fn trusted_roots(run: &Run) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = run
        .library
        .iter()
        .chain(run.models_ngspice.iter())
        .chain(run.models_xyce.iter())
        .filter_map(|p| p.canonicalize().ok())
        .collect();
    v.dedup();
    v
}

/// Paths xschem would see that it must not: their characters could be read
/// as Tcl.
pub fn unsafe_paths(run: &Run) -> Vec<PathBuf> {
    run.library
        .iter()
        .map(PathBuf::as_path)
        .chain(run.models_ngspice.as_deref())
        .chain(run.models_xyce.as_deref())
        .filter(|p| !safe_path(p))
        .map(Path::to_path_buf)
        .collect()
}

/// The name aispice gives the file it writes for xschem: a plain `.sch`
/// file name of letters, digits, `.`, `_` and `-`, never a path.
fn checked_name(name: &str) -> Result<&str, SimError> {
    let plain = !name.starts_with('.')
        && name.len() > 4
        && name.ends_with(".sch")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if plain {
        Ok(name)
    } else {
        Err(SimError::Config(format!(
            "xschem was not run: `{name}` is not a plain .sch file name (letters, digits, . _ - only)"
        )))
    }
}

/// The host folders one run uses. Where they appear to xschem depends on
/// the sandbox: bubblewrap remaps them to the [`SANDBOX_OUT`] family inside
/// its namespace, Landlock uses them as they are.
#[derive(Debug, Clone)]
pub struct Dirs {
    pub out: PathBuf,
    pub rc: PathBuf,
    pub home: PathBuf,
    pub tmp: PathBuf,
    /// The private folder holding the one file xschem netlists, written by
    /// aispice itself.
    pub staging: PathBuf,
}

/// The limits every xschem run gets, whichever sandbox is used.
fn limits() -> Confinement {
    Confinement {
        landlock: None,
        rlimit_fsize: Some(MAX_FILE),
        rlimit_as: Some(MAX_MEMORY),
    }
}

/// xschem's own arguments: headless (`-x`), netlist and quit (`-n -s -q`),
/// aispice's rc only (`-r --rcfile`), writing `netlist.spice` into `out`.
fn xschem_args(rc: &Path, out: &Path, sch: &Path) -> Vec<OsString> {
    vec![
        "-n".into(),
        "-s".into(),
        "-q".into(),
        "-x".into(),
        "-r".into(),
        "--rcfile".into(),
        rc.into(),
        "-o".into(),
        out.into(),
        "-N".into(),
        "netlist.spice".into(),
        sch.into(),
    ]
}

/// The environment one run gets: a fixed allowlist, nothing inherited, every
/// path a value the rc reads.
fn run_env(home: &Path, tmp: &Path, out: &Path, run: &Run) -> Vec<(String, OsString)> {
    let joined = run
        .library
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    let mut env: Vec<(String, OsString)> = vec![
        ("PATH".into(), "/usr/bin:/bin".into()),
        ("HOME".into(), home.into()),
        ("TMPDIR".into(), tmp.into()),
        ("LC_ALL".into(), "C".into()),
        ("AISPICE_XSCHEM_LIBRARY_PATH".into(), joined.into()),
        ("AISPICE_XSCHEM_NETLIST_DIR".into(), out.into()),
    ];
    if let Some(m) = &run.models_ngspice {
        env.push(("AISPICE_MODELS_NGSPICE".into(), m.into()));
    }
    if let Some(m) = &run.models_xyce {
        env.push(("AISPICE_MODELS_XYCE".into(), m.into()));
    }
    env
}

/// The binaries the Landlock domain lets xschem execute: itself and the
/// helpers it runs while netlisting (a shell, awk, gzip), plus the dynamic
/// loader, which the kernel opens for execution as every binary's
/// interpreter. Each is resolved to its real path so the pinned descriptor
/// is the binary, not a symlink. This keeps xschem from starting other
/// programs by path, but it is not a boundary on its own: the loader can
/// run any readable binary. What holds whatever code runs is the seccomp
/// filter and the Landlock file, network and scope rules.
fn exec_helpers(program: &Path) -> Vec<PathBuf> {
    let mut v = vec![program.to_path_buf()];
    for h in [
        "/bin/sh",
        "/usr/bin/awk",
        "/usr/bin/gzip",
        "/lib64/ld-linux-x86-64.so.2",
        "/lib/ld-linux-aarch64.so.1",
    ] {
        if let Ok(real) = Path::new(h).canonicalize()
            && !v.contains(&real)
        {
            v.push(real);
        }
    }
    v
}

/// The command for one netlist run, without running it. `dirs` are the host
/// folders aispice made for the run; `input` is the file it wrote there.
pub fn netlist_command(
    x: &Xschem,
    sandbox: &Sandbox,
    input: &Path,
    run: &Run,
    dirs: &Dirs,
) -> ProcessSpec {
    let program = x
        .program
        .canonicalize()
        .unwrap_or_else(|_| x.program.clone());
    let trusted = trusted_roots(run);
    match sandbox {
        Sandbox::Bwrap(bwrap) => {
            // bubblewrap's own network, IPC and PID namespaces stand in for
            // seccomp and Landlock; it shows xschem the private input folder
            // and the trusted roots read-only.
            let mut read_only: Vec<PathBuf> = vec![program.clone(), dirs.staging.clone()];
            read_only.extend(x.share.iter().cloned());
            read_only.extend(trusted.iter().cloned());
            read_only.retain(|p| p.exists());
            let view = View {
                read_only,
                out: dirs.out.clone(),
                rc: dirs.rc.clone(),
            };
            let env = run_env(
                Path::new(SANDBOX_HOME),
                Path::new("/tmp"),
                Path::new(SANDBOX_OUT),
                run,
            );
            let args = xschem_args(Path::new(SANDBOX_RC), Path::new(SANDBOX_OUT), input);
            let mut spec = ProcessSpec::new(bwrap, &dirs.out);
            spec.args = bwrap_args(&view, &env, &program, &args);
            spec.clear_env = true;
            spec.confine = Some(limits());
            spec
        }
        Sandbox::Landlock { .. } => {
            // xschem runs directly, confined before it starts: read the
            // system, the library, the PDK and its input; execute only itself
            // and its helpers; write only the private folders.
            let mut read: Vec<PathBuf> = SYSTEM.iter().map(PathBuf::from).collect();
            read.extend(SYSTEM_TRY.iter().map(PathBuf::from));
            read.extend(["/dev/urandom", "/dev/zero"].iter().map(PathBuf::from));
            read.extend(x.share.iter().cloned());
            read.extend(trusted.iter().cloned());
            read.push(dirs.staging.clone());
            read.push(dirs.rc.clone());
            let write = vec![dirs.out.clone(), dirs.home.clone(), dirs.tmp.clone()];
            let env = run_env(&dirs.home, &dirs.tmp, &dirs.out, run);
            let args = xschem_args(&dirs.rc, &dirs.out, input);
            let mut spec = ProcessSpec::new(&program, &dirs.out);
            spec.args = args;
            spec.env = env
                .into_iter()
                .map(|(k, v)| (OsString::from(k), v))
                .collect();
            spec.clear_env = true;
            let mut confine = limits();
            confine.landlock = Some(Landlock {
                read,
                exec: exec_helpers(&program),
                write,
                rw: vec![PathBuf::from("/dev/null")],
            });
            spec.confine = Some(confine);
            spec
        }
    }
}

/// Create a directory only its owner may use.
fn make_private_dir(p: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(p)
    }
    #[cfg(not(unix))]
    std::fs::create_dir(p)
}

/// Write `bytes` to a file that must not exist yet: `create_new` fails on
/// anything already there, a symlink included.
fn write_new(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)?
        .write_all(bytes)
}

/// Read the netlist xschem wrote at `path`. The folder was writable inside
/// the sandbox, and a process it left behind could still change it, so the
/// file is opened once without following a symlink and everything else (it
/// is a plain file, its size, its text) is read through that one descriptor,
/// never by the path again.
fn read_netlist(path: &Path, log: &str) -> Result<String, SimError> {
    use std::io::Read as _;
    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    };
    #[cfg(not(unix))]
    let opened = std::fs::File::open(path);
    let file = match opened {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(SimError::NoOutput(format!(
                "xschem wrote no netlist{}",
                first_line(log)
                    .map(|l| format!(": {l}"))
                    .unwrap_or_default()
            )));
        }
        Err(_) => {
            return Err(SimError::NoOutput(
                "xschem's netlist is not a plain file".into(),
            ));
        }
    };
    let meta = file.metadata().map_err(|e| SimError::Io(e.to_string()))?;
    if !meta.is_file() {
        return Err(SimError::NoOutput(
            "xschem's netlist is not a plain file".into(),
        ));
    }
    let too_large = |size| SimError::TooLarge {
        what: "xschem's netlist".into(),
        size,
        limit: MAX_NETLIST,
    };
    if meta.len() > MAX_NETLIST {
        return Err(too_large(meta.len()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_NETLIST + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| SimError::Io(e.to_string()))?;
    if bytes.len() as u64 > MAX_NETLIST {
        return Err(too_large(bytes.len() as u64));
    }
    String::from_utf8(bytes).map_err(|_| SimError::NoOutput("xschem's netlist is not text".into()))
}

/// Netlist `text`, a schematic aispice wrote itself, with xschem inside
/// `sandbox`, and return the netlist text. There is no path parameter on
/// purpose: aispice writes the file into a fresh private folder (`name` is
/// only its file name), so xschem never opens a file it did not write. The
/// caller picks the sandbox with [`Sandbox::select`].
pub async fn netlist(
    x: &Xschem,
    sandbox: &Sandbox,
    name: &str,
    text: &str,
    run: &Run,
    cancel: &CancellationToken,
) -> Result<String, SimError> {
    let name = checked_name(name)?;
    if let Some(p) = unsafe_paths(run).first() {
        return Err(SimError::Config(format!(
            "xschem was not run: the path {} has characters xschem would read as Tcl (only letters, digits and / . _ - + , = @ are passed to it)",
            p.display()
        )));
    }
    // tempfile makes the run folder readable by its owner only.
    let tmp = tempfile::tempdir().map_err(|e| SimError::Io(e.to_string()))?;
    let dirs = Dirs {
        out: tmp.path().join("out"),
        rc: tmp.path().join("aispicerc.tcl"),
        home: tmp.path().join("home"),
        tmp: tmp.path().join("tmp"),
        staging: tmp.path().join("in"),
    };
    for d in [&dirs.out, &dirs.home, &dirs.tmp] {
        std::fs::create_dir(d).map_err(|e| SimError::Io(e.to_string()))?;
    }
    make_private_dir(&dirs.staging).map_err(|e| SimError::Io(e.to_string()))?;
    write_new(&dirs.rc, RC.as_bytes()).map_err(|e| SimError::Io(e.to_string()))?;
    let input = dirs.staging.join(name);
    write_new(&input, text.as_bytes()).map_err(|e| SimError::Io(e.to_string()))?;
    let spec = netlist_command(x, sandbox, &input, run, &dirs);
    let result = process::run(&spec, TIMEOUT, cancel).await?;
    read_netlist(&dirs.out.join("netlist.spice"), &result.combined())
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
    fn paths_and_names_with_tcl_characters_are_refused() {
        let run = Run {
            library: vec![
                PathBuf::from("/usr/share/xschem/xschem_library"),
                PathBuf::from("/tmp/a [x]"),
            ],
            ..Run::default()
        };
        assert_eq!(unsafe_paths(&run), [PathBuf::from("/tmp/a [x]")]);
        assert!(checked_name("tb.sch").is_ok());
        for bad in [
            "a b.sch",
            "../tb.sch",
            "x/tb.sch",
            ".sch",
            ".hidden.sch",
            "tb.txt",
            "$x.sch",
            "a[1].sch",
        ] {
            assert!(checked_name(bad).is_err(), "{bad}");
        }
    }

    // Used only by the command-shape tests, which are Unix-only.
    #[cfg(unix)]
    fn dirs(dir: &Path) -> Dirs {
        Dirs {
            out: dir.join("out"),
            rc: dir.join("rc.tcl"),
            home: dir.join("home"),
            tmp: dir.join("tmp"),
            staging: dir.join("in"),
        }
    }

    #[tokio::test]
    async fn a_bad_file_name_never_starts_xschem() {
        let tmp = tempfile::tempdir().unwrap();
        // A program that does not exist: the refusal (the space in the name)
        // comes first.
        let x = Xschem {
            program: tmp.path().join("no-xschem"),
            share: None,
        };
        let sandbox = Sandbox::Bwrap(tmp.path().join("no-bwrap"));
        let err = netlist(
            &x,
            &sandbox,
            "a b.sch",
            "v {xschem version=3.4.4 file_version=1.2}\n",
            &Run::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("xschem was not run"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_bwrap_that_cannot_start_does_not_run_xschem() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let sentinel = dir.join("ran");
        // A stand-in xschem that leaves a mark, and a bwrap that fails before
        // it could exec it: handed Sandbox::Bwrap, the run fails and the
        // mark is never made.
        let fake = dir.join("xschem");
        std::fs::write(&fake, format!("#!/bin/sh\ntouch {}\n", sentinel.display())).unwrap();
        let broken = dir.join("bwrap");
        std::fs::write(
            &broken,
            "#!/bin/sh\necho 'bwrap: no namespaces' >&2\nexit 1\n",
        )
        .unwrap();
        for p in [&fake, &broken] {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let x = Xschem {
            program: fake,
            share: None,
        };
        let err = netlist(
            &x,
            &Sandbox::Bwrap(broken),
            "tb.sch",
            "v {xschem version=3.4.4 file_version=1.2}\n",
            &Run::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("xschem"), "{err}");
        assert!(!sentinel.exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_bwrap_command_shows_only_what_the_run_needs() {
        let x = Xschem {
            program: PathBuf::from("/usr/bin/xschem"),
            share: Some(PathBuf::from("/usr/share/xschem")),
        };
        let sandbox = Sandbox::Bwrap(PathBuf::from("/usr/bin/bwrap"));
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let pdk = dir.join("pdk/libs.tech/xschem");
        std::fs::create_dir_all(&pdk).unwrap();
        let run = Run {
            library: vec![
                PathBuf::from("/usr/share/xschem/xschem_library"),
                pdk.clone(),
            ],
            ..Run::default()
        };
        let d = dirs(&dir);
        std::fs::create_dir_all(&d.staging).unwrap();
        let sch = d.staging.join("tb.sch");
        std::fs::write(&sch, "").unwrap();
        let spec = netlist_command(&x, &sandbox, &sch, &run, &d);
        assert_eq!(spec.program, PathBuf::from("/usr/bin/bwrap"));
        assert!(spec.clear_env && spec.env.is_empty());
        // The limits ride along, now set in the pre_exec hook, not prlimit.
        let confine = spec.confine.as_ref().unwrap();
        assert!(confine.rlimit_fsize.is_some() && confine.rlimit_as.is_some());
        assert!(confine.landlock.is_none());
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let has = |w: &[&str]| args.windows(w.len()).any(|x| x == w);
        for flag in [
            "--unshare-all",
            "--unshare-user",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ] {
            assert!(args.contains(&flag.to_string()), "{flag}: {args:?}");
        }
        // Read-only views of the system, the PDK and the private folder
        // holding the input; the one writable folder; a private, bounded
        // /tmp and home.
        assert!(has(&["--ro-bind", "/usr", "/usr"]));
        let s = |p: &Path| p.display().to_string();
        assert!(has(&["--ro-bind", &s(&pdk), &s(&pdk)]));
        assert!(has(&["--ro-bind", &s(&d.staging), &s(&d.staging)]));
        assert!(has(&["--ro-bind", &s(&d.rc), SANDBOX_RC]));
        assert!(has(&["--bind", &s(&d.out), SANDBOX_OUT]));
        assert!(has(&["--tmpfs", "/tmp"]) && has(&["--dir", SANDBOX_HOME]));
        assert!(has(&["--chdir", SANDBOX_OUT]));
        assert!(has(&["--setenv", "HOME", SANDBOX_HOME]));
        // The only writable bind is the output folder; the user's home and
        // the working folder are not shown.
        assert_eq!(args.iter().filter(|a| *a == "--bind").count(), 1);
        if let Some(home) = std::env::var_os("HOME") {
            let home = home.to_string_lossy().into_owned();
            assert!(!has(&["--ro-bind", &home, &home]));
        }
        let cwd = std::env::current_dir().unwrap().display().to_string();
        assert!(!args.contains(&cwd), "{args:?}");
        // xschem itself: headless, with aispice's rc only, the schematic last.
        let tail = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[tail + 1], "/usr/bin/xschem");
        let rest = &args[tail + 2..];
        assert!(rest.windows(2).any(|w| w == ["--rcfile", SANDBOX_RC]));
        assert!(rest.contains(&"-x".to_string()) && rest.contains(&"-q".to_string()));
        assert_eq!(rest.last().unwrap(), &s(&sch));
    }

    /// The Landlock plan: xschem runs directly, confined to read and execute
    /// the system and its install, read the inputs, and write only the
    /// private folders, never the input's folder or the user's home.
    #[cfg(unix)]
    #[test]
    fn the_landlock_plan_confines_to_what_the_run_needs() {
        let x = Xschem {
            program: PathBuf::from("/usr/bin/xschem"),
            share: Some(PathBuf::from("/usr/share/xschem")),
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let run = Run {
            library: vec![PathBuf::from("/usr/share/xschem/xschem_library")],
            models_ngspice: Some(dir.join("models")),
            ..Run::default()
        };
        let d = dirs(&dir);
        let sch = d.staging.join("tb.sch");
        let spec = netlist_command(&x, &Sandbox::Landlock { abi: 6 }, &sch, &run, &d);
        // xschem runs directly, with the environment aispice sets.
        assert_eq!(spec.program, PathBuf::from("/usr/bin/xschem"));
        assert!(spec.clear_env);
        let env: std::collections::HashMap<_, _> = spec
            .env
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        assert_eq!(env["HOME"], d.home.display().to_string());
        assert_eq!(env["TMPDIR"], d.tmp.display().to_string());
        assert_eq!(
            env["AISPICE_XSCHEM_NETLIST_DIR"],
            d.out.display().to_string()
        );
        // The environment is a fixed allowlist: nothing that could point the
        // process at the user's session is carried over.
        for leaked in [
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "SSH_AUTH_SOCK",
        ] {
            assert!(!env.contains_key(leaked), "{leaked} reached the sandbox");
        }
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w == ["--rcfile", &d.rc.display().to_string()])
        );
        assert_eq!(args.last().unwrap(), &sch.display().to_string());
        let ll = spec.confine.as_ref().unwrap().landlock.as_ref().unwrap();
        // Writable: only the private output, home and tmp, never the input's
        // folder or the user's home; /dev/null is read-write.
        assert!(
            ll.write.contains(&d.out) && ll.write.contains(&d.home) && ll.write.contains(&d.tmp)
        );
        assert!(!ll.write.contains(&dir));
        assert!(ll.rw.contains(&PathBuf::from("/dev/null")));
        if let Some(home) = std::env::var_os("HOME") {
            assert!(!ll.write.contains(&PathBuf::from(home)));
        }
        // Readable: the system and the private copy. Executable: only xschem
        // and its helpers, never a directory of binaries like /usr/bin.
        assert!(ll.read.contains(&PathBuf::from("/usr")));
        assert!(ll.read.contains(&d.staging));
        assert!(ll.exec.contains(&PathBuf::from("/usr/bin/xschem")));
        assert!(
            !ll.exec.contains(&PathBuf::from("/usr/bin"))
                && !ll.exec.contains(&PathBuf::from("/usr"))
        );
    }

    /// A sandbox that starts on this machine, or `None` with a note.
    async fn working_sandbox() -> Option<Sandbox> {
        match Sandbox::select(&CancellationToken::new()).await {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("skipped: no sandbox here: {e}");
                None
            }
        }
    }

    /// Gated on xschem and a working sandbox: a small schematic netlists,
    /// with no window, without reading the user's rc or an `xschemrc` in
    /// the working folder.
    #[tokio::test]
    async fn netlists_headless_in_the_sandbox() {
        let Some(x) = Xschem::find().filter(|x| x.share.is_some()) else {
            eprintln!("skipped: xschem is not installed");
            return;
        };
        let Some(sandbox) = working_sandbox().await else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let sch = "v {xschem version=3.4.4 file_version=1.2}\nN 0 -30 0 -60 {}\nC {res.sym} 0 0 0 0 {name=R1 value=1k}\nC {gnd.sym} 0 30 0 0 {name=l1 lab=GND}\nC {lab_pin.sym} 0 -60 0 0 {name=p1 lab=out}\n";
        // An rc in the working folder would leave a mark if xschem sourced it.
        let mark = dir.join("rc-was-read");
        let rc = format!("exec touch {}\n", mark.display());
        let cwd = std::env::current_dir().unwrap();
        let cwd_rc = cwd.join("xschemrc");
        let wrote_cwd_rc = !cwd_rc.exists() && std::fs::write(&cwd_rc, &rc).is_ok();
        let run = Run {
            library: x.library(),
            ..Run::default()
        };
        let text = netlist(&x, &sandbox, "rc.sch", sch, &run, &CancellationToken::new()).await;
        if wrote_cwd_rc {
            let _ = std::fs::remove_file(&cwd_rc);
        }
        let text = text.unwrap();
        // The resistor netlists between the named nets (xschem versions
        // differ on whether they add `m=1`, so do not require it here).
        assert!(text.contains("R1 out GND 1k"), "{text}");
        assert!(!mark.exists());
    }

    /// Write a stand-in in xschem's place that runs `script`, run it in
    /// `sandbox` on an empty schematic, and return its netlist.
    #[cfg(unix)]
    async fn run_stand_in(sandbox: &Sandbox, dir: &Path, script: &str) -> Result<String, SimError> {
        use std::os::unix::fs::PermissionsExt;
        let fake = dir.join("fake-xschem");
        std::fs::write(&fake, script).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let x = Xschem {
            program: fake,
            share: None,
        };
        netlist(
            &x,
            sandbox,
            "tb.sch",
            "",
            &Run::default(),
            &CancellationToken::new(),
        )
        .await
    }

    /// Gated on a sandbox that starts on this machine: whatever the sandbox
    /// is, a stand-in in xschem's place writes only its netlist and cannot,
    /// from inside, create a file outside its private folder or read a file
    /// outside what the run allows. Each attempt is benign and records
    /// nothing on success, so the netlist staying exactly `ok` is the proof
    /// that none worked.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_sandbox_holds() {
        let Some(sandbox) = working_sandbox().await else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        // A file the run does not allow reading, beside the input.
        let decoy = dir.join("decoy");
        std::fs::write(&decoy, "SECRET\n").unwrap();
        // Marks outside the one writable folder: the input's folder, the
        // user's home, the real /tmp.
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()));
        let tag = format!("aispice-confinement-{}", std::process::id());
        let marks = [
            dir.join(&tag),
            home.join(&tag),
            PathBuf::from("/tmp").join(&tag),
        ];
        let mut script = String::from("#!/bin/sh\nprintf 'ok\\n' > netlist.spice\n");
        for m in &marks {
            script.push_str(&format!("touch '{}' 2>/dev/null\n", m.display()));
        }
        script.push_str(&format!(
            "cat '{}' >> netlist.spice 2>/dev/null\n",
            decoy.display()
        ));
        let text = run_stand_in(&sandbox, &dir, &script).await.unwrap();
        let escaped: Vec<&PathBuf> = marks.iter().filter(|m| m.exists()).collect();
        for m in &escaped {
            let _ = std::fs::remove_file(m);
        }
        assert!(escaped.is_empty(), "wrote outside the sandbox: {escaped:?}");
        // No decoy content: the netlist is only what the stand-in wrote.
        assert_eq!(text, "ok\n", "sandbox let something through");
    }

    /// Gated on Landlock being the sandbox here: from inside, a stand-in
    /// cannot execute a binary that is not xschem or one of its helpers, and
    /// cannot signal a process outside its domain. (bubblewrap covers these
    /// with its own namespaces, so this is checked only under Landlock.)
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_landlock_sandbox_narrows_exec_and_signals() {
        let Some(sandbox) = working_sandbox().await else {
            return;
        };
        if !matches!(sandbox, Sandbox::Landlock { .. }) {
            eprintln!("skipped: the sandbox here is not Landlock");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        // env, if it could run, would print the environment into the netlist;
        // kill -0 of this test process, if allowed, would append `signalled`.
        // Under Landlock both are denied, so the netlist stays `ok`.
        let script = format!(
            "#!/bin/sh\nprintf 'ok\\n' > netlist.spice\n\
             /usr/bin/env >> netlist.spice 2>/dev/null\n\
             kill -0 {} 2>/dev/null && printf 'signalled\\n' >> netlist.spice\n",
            std::process::id()
        );
        let text = run_stand_in(&sandbox, &dir, &script).await.unwrap();
        assert_eq!(text, "ok\n", "exec or signal was not denied");
    }

    /// The netlist is read through one descriptor opened without following
    /// a symlink: one left (or swapped in) at its place by a process in the
    /// sandbox is refused, and the file it points to is never read.
    #[cfg(unix)]
    #[test]
    fn a_netlist_that_is_a_symlink_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let decoy = dir.join("decoy");
        std::fs::write(&decoy, "SECRET\n").unwrap();
        let path = dir.join("netlist.spice");
        std::os::unix::fs::symlink(&decoy, &path).unwrap();
        let err = read_netlist(&path, "").unwrap_err().to_string();
        assert!(err.contains("not a plain file"), "{err}");
        assert!(!err.contains("SECRET"));
        // A plain file reads, and a missing one says xschem wrote nothing.
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, "R1 a b 1k\n").unwrap();
        assert_eq!(read_netlist(&path, "").unwrap(), "R1 a b 1k\n");
        std::fs::remove_file(&path).unwrap();
        assert!(
            read_netlist(&path, "boom")
                .unwrap_err()
                .to_string()
                .contains("wrote no netlist: boom")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_landlock_abi_floor_is_enforced() {
        assert!(landlock_choice_from(0).is_err());
        assert!(landlock_choice_from(crate::backend::confine::MIN_ABI - 1).is_err());
        assert_eq!(
            landlock_choice_from(crate::backend::confine::MIN_ABI),
            Ok(crate::backend::confine::MIN_ABI)
        );
    }
}
