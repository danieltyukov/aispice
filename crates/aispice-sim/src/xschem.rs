//! Running xschem as a netlister on a schematic aispice wrote itself.
//!
//! xschem evaluates Tcl from the files it opens: property values, symbol
//! formats and texts, symbols it loads by library path, and the rc files it
//! sources. So aispice never gives it a file it did not write: [`netlist`]
//! takes an export's text ([`SchText`], which only the export writer makes,
//! keeping every value literal), not a path, and writes it into a fresh
//! private folder itself. The only caller in the product is the check of an
//! export; an imported schematic is never run through xschem.
//!
//! The run is sandboxed as defense in depth, in whichever sandbox
//! [`Sandbox::select`] finds ([`crate::sandbox`]): bubblewrap where it can
//! start, else Landlock with seccomp's [`Profile::Denylist`], else not at
//! all, and the caller reports the file as not verified with xschem. xschem
//! may read the system, its own install, the library and model folders, its
//! input and the rc; execute only itself, a shell, awk and gzip; and write
//! only its private output, home and tmp folders. It gets a file size,
//! address space and process count limit, keeps no descriptor of aispice's
//! but stdin, stdout and stderr, dies with aispice, and its process group
//! is killed when it exits. It is run:
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
//!   `/ . _ - + , = @` (xschem splices file names into Tcl commands): the
//!   library and model folders, and the private folders aispice makes
//!   under `TMPDIR`.
//!
//! The netlist it writes is read back through one descriptor opened without
//! following a symlink and without blocking (a fifo left in its place
//! cannot stall the read), so nothing left behind in the writable folder
//! can point the read elsewhere.

use crate::backend::process::{self, ProcessSpec};
use crate::backend::{SimError, which};
pub use crate::sandbox::Sandbox;
use crate::sandbox::{Limits, Plan, Profile};
pub use aispice_core::xschem::export::SchText;
use aispice_core::xschem::safety::safe_path;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A netlist run takes about a tenth of a second; this is generous.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Netlists larger than this are not read.
const MAX_NETLIST: u64 = 16 << 20;
/// No file xschem writes may grow past this (`RLIMIT_FSIZE`).
const MAX_FILE: u64 = 64 << 20;
/// xschem's address space (`RLIMIT_AS`).
const MAX_MEMORY: u64 = 4 << 30;
/// Processes and threads xschem and its helpers may add to what the user
/// runs (`RLIMIT_NPROC`, relative): a shell, awk and gzip need a handful.
const NPROC_HEADROOM: u64 = 128;

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

/// The host folders one run uses, the same under either sandbox.
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
fn limits() -> Limits {
    Limits {
        file_size: Some(MAX_FILE),
        address_space: Some(MAX_MEMORY),
        nproc_headroom: Some(NPROC_HEADROOM),
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

/// The helpers xschem runs while netlisting: a shell, awk, gzip. The
/// sandbox adds xschem itself and the loader.
const HELPERS: &[&str] = &["/bin/sh", "/usr/bin/awk", "/usr/bin/gzip"];

/// What one netlist run may touch. `dirs` are the folders aispice made for
/// the run; `input` is the file it wrote there.
pub fn netlist_plan(x: &Xschem, input: &Path, run: &Run, dirs: &Dirs) -> Plan {
    let mut read: Vec<PathBuf> = x.share.iter().cloned().collect();
    read.extend(trusted_roots(run));
    read.push(dirs.staging.clone());
    read.push(dirs.rc.clone());
    Plan {
        program: x.program.clone(),
        args: xschem_args(&dirs.rc, &dirs.out, input),
        env: run_env(&dirs.home, &dirs.tmp, &dirs.out, run)
            .into_iter()
            .map(|(k, v)| (OsString::from(k), v))
            .collect(),
        cwd: dirs.out.clone(),
        read,
        exec: HELPERS
            .iter()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .collect(),
        write: vec![dirs.out.clone(), dirs.home.clone(), dirs.tmp.clone()],
        profile: Profile::Denylist,
        limits: limits(),
    }
}

/// The command for one netlist run in `sandbox`, without running it.
pub fn netlist_command(
    x: &Xschem,
    sandbox: &Sandbox,
    input: &Path,
    run: &Run,
    dirs: &Dirs,
) -> Result<ProcessSpec, SimError> {
    sandbox.command(&netlist_plan(x, input, run, dirs))
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
/// never by the path again. The open does not block, so a fifo made in its
/// place (bubblewrap's writable folder allows one) is refused as not a
/// plain file instead of stalling the read; a plain file reads the same.
fn read_netlist(path: &Path, log: &str) -> Result<String, SimError> {
    use std::io::Read as _;
    #[cfg(unix)]
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
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

/// The first host path one run hands xschem that has characters xschem
/// would read as Tcl. The run's folders sit under `TMPDIR`, which aispice
/// does not choose; under Landlock xschem sees all of them, under
/// bubblewrap the input at its own path.
fn unsafe_run_path<'p>(dirs: &'p Dirs, input: &'p Path) -> Option<&'p Path> {
    [
        dirs.out.as_path(),
        dirs.rc.as_path(),
        dirs.home.as_path(),
        dirs.tmp.as_path(),
        dirs.staging.as_path(),
        input,
    ]
    .into_iter()
    .find(|p| !safe_path(p))
}

/// Netlist `text`, an export aispice wrote itself, with xschem inside
/// `sandbox`, and return the netlist text. There is no path parameter on
/// purpose: aispice writes the file into a fresh private folder (`name` is
/// only its file name), so xschem never opens a file it did not write. The
/// caller picks the sandbox with [`Sandbox::select`].
pub async fn netlist(
    x: &Xschem,
    sandbox: &Sandbox,
    name: &str,
    text: &SchText,
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
    let input = dirs.staging.join(name);
    if let Some(p) = unsafe_run_path(&dirs, &input) {
        return Err(SimError::Config(format!(
            "xschem was not run: its private folder {} has characters xschem would read as Tcl; set TMPDIR to a folder of letters, digits and / . _ - + , = @",
            p.display()
        )));
    }
    for d in [&dirs.out, &dirs.home, &dirs.tmp] {
        std::fs::create_dir(d).map_err(|e| SimError::Io(e.to_string()))?;
    }
    make_private_dir(&dirs.staging).map_err(|e| SimError::Io(e.to_string()))?;
    write_new(&dirs.rc, RC.as_bytes()).map_err(|e| SimError::Io(e.to_string()))?;
    write_new(&input, text.as_str().as_bytes()).map_err(|e| SimError::Io(e.to_string()))?;
    let spec = netlist_command(x, sandbox, &input, run, &dirs)?;
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
        let sandbox = Sandbox::bwrap_for_tests(tmp.path().join("no-bwrap"));
        let err = netlist(
            &x,
            &sandbox,
            "a b.sch",
            &SchText::trusted_for_tests("v {xschem version=3.4.4 file_version=1.2}\n"),
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
            &Sandbox::bwrap_for_tests(broken),
            "tb.sch",
            &SchText::trusted_for_tests("v {xschem version=3.4.4 file_version=1.2}\n"),
            &Run::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        // Off Linux the confined run is refused before bwrap starts, with
        // an error that names bwrap rather than xschem.
        if cfg!(target_os = "linux") {
            assert!(err.to_string().contains("xschem"), "{err}");
        }
        assert!(!sentinel.exists());
    }

    /// What a netlist run may touch, the same under either sandbox: read
    /// xschem's install, the library and model folders, its input and rc;
    /// execute only xschem and its helpers; write only the private output,
    /// home and tmp, never the input's folder or the user's home; an
    /// environment of a fixed allowlist; aispice's rc only, headless, the
    /// schematic last.
    #[cfg(unix)]
    #[test]
    fn the_netlist_plan_shows_only_what_the_run_needs() {
        let x = Xschem {
            program: PathBuf::from("/usr/bin/xschem"),
            share: Some(PathBuf::from("/usr/share/xschem")),
        };
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().canonicalize().unwrap();
        let pdk = dir.join("pdk/libs.tech/xschem");
        std::fs::create_dir_all(&pdk).unwrap();
        let run = Run {
            library: vec![
                PathBuf::from("/usr/share/xschem/xschem_library"),
                pdk.clone(),
            ],
            models_ngspice: Some(dir.join("models")),
            ..Run::default()
        };
        let d = dirs(&dir);
        let sch = d.staging.join("tb.sch");
        let plan = netlist_plan(&x, &sch, &run, &d);
        assert_eq!(plan.program, PathBuf::from("/usr/bin/xschem"));
        assert_eq!(plan.profile, Profile::Denylist);
        assert_eq!(plan.limits, limits());
        assert_eq!(plan.cwd, d.out);
        assert_eq!(
            plan.write,
            vec![d.out.clone(), d.home.clone(), d.tmp.clone()]
        );
        assert!(!plan.write.contains(&dir));
        if let Some(home) = std::env::var_os("HOME") {
            assert!(!plan.write.contains(&PathBuf::from(home)));
        }
        for r in [&d.staging, &d.rc, &pdk, &PathBuf::from("/usr/share/xschem")] {
            assert!(plan.read.contains(r), "{r:?}: {:?}", plan.read);
        }
        assert!(
            plan.exec
                .iter()
                .all(|p| HELPERS.contains(&p.to_str().unwrap()))
        );
        let env: std::collections::HashMap<_, _> = plan
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
        for leaked in [
            "DBUS_SESSION_BUS_ADDRESS",
            "XDG_RUNTIME_DIR",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "SSH_AUTH_SOCK",
        ] {
            assert!(!env.contains_key(leaked), "{leaked} reached the sandbox");
        }
        let args: Vec<String> = plan
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w == ["--rcfile", &d.rc.display().to_string()])
        );
        assert!(args.contains(&"-x".to_string()) && args.contains(&"-q".to_string()));
        assert_eq!(args.last().unwrap(), &sch.display().to_string());
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
        for sandbox in crate::sandbox::every_available().await {
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
            let text = netlist(
                &x,
                &sandbox,
                "rc.sch",
                &SchText::trusted_for_tests(sch),
                &run,
                &CancellationToken::new(),
            )
            .await;
            if wrote_cwd_rc {
                let _ = std::fs::remove_file(&cwd_rc);
            }
            let text = text.unwrap();
            // The resistor netlists between the named nets (xschem versions
            // differ on whether they add `m=1`, so do not require it here).
            assert!(
                text.contains("R1 out GND 1k"),
                "{}: {text}",
                sandbox.describe()
            );
            assert!(!mark.exists());
        }
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
            &SchText::trusted_for_tests(""),
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
        for sandbox in crate::sandbox::every_available().await {
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
            assert_eq!(
                text,
                "ok\n",
                "{}: let something through",
                sandbox.describe()
            );
        }
    }

    /// Gated on Landlock being the sandbox here: from inside, a stand-in
    /// cannot execute a binary that is not xschem or one of its helpers, and
    /// cannot signal a process outside its domain. (bubblewrap covers these
    /// with its own namespaces, so this is checked only under Landlock.)
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_landlock_sandbox_narrows_exec_and_signals() {
        for sandbox in crate::sandbox::every_available().await {
            if sandbox.is_bwrap() {
                continue;
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
            assert_eq!(
                text,
                "ok\n",
                "{}: exec or signal was not denied",
                sandbox.describe()
            );
        }
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

    /// A fifo left at the netlist's place (bubblewrap's writable folder
    /// allows one) is refused at once, not waited on for a writer that
    /// never comes.
    #[cfg(unix)]
    #[test]
    fn a_netlist_that_is_a_fifo_does_not_block() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("netlist.spice");
        let made = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success());
        let (tx, rx) = std::sync::mpsc::channel();
        let p = path.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_netlist(&p, "").map_err(|e| e.to_string()));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("reading a fifo netlist blocked");
        assert!(got.unwrap_err().contains("not a plain file"));
    }

    /// The run's private folders come from TMPDIR; one with characters xschem
    /// would read as Tcl stops the run before xschem starts.
    #[cfg(unix)]
    #[test]
    fn a_private_folder_with_tcl_characters_is_refused() {
        let ok = dirs_at(Path::new("/tmp/.tmpAbC123"));
        assert_eq!(unsafe_run_path(&ok, &ok.staging.join("tb.sch")), None);
        for bad in ["/tmp/a b", "/tmp/[x]", "/tmp/$HOME", "/tmp/a;b", "/tmp/{x}"] {
            let d = dirs_at(Path::new(bad));
            assert!(
                unsafe_run_path(&d, &d.staging.join("tb.sch")).is_some(),
                "{bad}"
            );
        }
    }

    #[cfg(unix)]
    fn dirs_at(dir: &Path) -> Dirs {
        Dirs {
            out: dir.join("out"),
            rc: dir.join("aispicerc.tcl"),
            home: dir.join("home"),
            tmp: dir.join("tmp"),
            staging: dir.join("in"),
        }
    }
}
