//! Running a program aispice does not trust: what it may touch
//! ([`policy`]), and where it runs ([`Sandbox`]).
//!
//! A caller describes one run as a [`Plan`]: host paths, the whole
//! environment, the seccomp [`Profile`] and the [`Limits`]. [`Sandbox::select`]
//! picks the sandbox this machine can give, and [`Sandbox::command`] turns
//! the plan into a command for [`crate::backend::process::run`]:
//!
//! - **bubblewrap** 0.8 or later, when it can make its namespaces here: new
//!   user, PID, IPC, UTS and network namespaces, no user namespace inside
//!   (`--disable-userns`, checked with `--assert-userns-disabled`), the
//!   system folders and the plan's `read` and `exec` paths read-only, its
//!   `write` folders read-write, a private size-capped `/tmp`, the plan's
//!   environment only, and the profile's seccomp programs installed by
//!   bubblewrap (`--add-seccomp-fd`). Every path appears at its own place,
//!   so the arguments and environment are the same as under Landlock. The
//!   PID namespace keeps signals in.
//! - else **Landlock**, at the kernel's ABI up to 6 (4 at least), with the
//!   profile's seccomp programs. At ABI 6 Landlock scopes signals and
//!   abstract sockets; at 4 or 5 seccomp keeps signals to the process and
//!   its group ([`Signals::SelfOnly`]), and every profile refuses sockets.
//!
//! [`Profile::NativeCode`] gets [`Signals::SelfOnly`] in every sandbox, at
//! every ABI: a scope or namespace keeps a signal from arriving, but only
//! seccomp makes `kill(-1, 0)` fail rather than report success.
//!   This is the path on a kernel that restricts unprivileged user
//!   namespaces without a bwrap AppArmor profile (Ubuntu 24.04's default).
//! - else nothing runs, and `select` says why.
//!
//! Either way the process keeps no descriptor of aispice's beyond stdin,
//! stdout and stderr, gets the plan's limits, dies with aispice, and its
//! process group is killed when it exits.

pub mod policy;

pub use policy::{
    Confinement, ConfinementBuilder, LandlockPolicy, Limits, MAX_LANDLOCK_ABI, MIN_LANDLOCK_ABI,
    PolicyError, Profile, SCOPED_LANDLOCK_ABI, Scopes, SeccompPolicy, Signals,
};

use crate::backend::process::{self, ProcessSpec};
use crate::backend::{SimError, which};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The oldest bubblewrap aispice runs: the first with `--disable-userns`
/// and `--size`; `--add-seccomp-fd` is older.
pub const MIN_BWRAP: (u32, u32, u32) = (0, 8, 0);
/// bubblewrap's private `/tmp` holds at most this much.
const TMP_BYTES: u64 = 64 << 20;

/// System folders shown read-only: programs, libraries, interpreters. The
/// usual top-level links into `/usr` are recreated as links.
const SYSTEM: &[&str] = &["/usr"];
const SYSTEM_LINKS: &[&str] = &["/bin", "/sbin", "/lib", "/lib64", "/lib32", "/libx32"];
/// Single system files and folders the loader needs, when present.
const SYSTEM_TRY: &[&str] = &[
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/alternatives",
];
/// System files a program may read under Landlock (bubblewrap makes its
/// own `/dev` and `/proc`): random bytes, zeros, and the memory and CPU
/// totals numeric programs size themselves by.
const DEVICES: &[&str] = &[
    "/dev/urandom",
    "/dev/zero",
    "/proc/meminfo",
    "/sys/devices/system/cpu/online",
];
/// The dynamic loaders the kernel opens for execution as every binary's
/// interpreter, so they need execute rights under Landlock.
const LOADERS: &[&str] = &["/lib64/ld-linux-x86-64.so.2", "/lib/ld-linux-aarch64.so.1"];

/// One program's run, before a sandbox is chosen. Paths are host paths.
#[derive(Debug, Clone)]
pub struct Plan {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// The program's whole environment: nothing of aispice's is inherited.
    pub env: Vec<(OsString, OsString)>,
    /// The working folder, one of `write`.
    pub cwd: PathBuf,
    /// Files and folders it may read besides the system folders.
    pub read: Vec<PathBuf>,
    /// Programs it may execute besides `program`, such as a script's
    /// interpreter or helpers it starts. The loader is added.
    pub exec: Vec<PathBuf>,
    /// Folders it may write, plain files and folders only.
    pub write: Vec<PathBuf>,
    pub profile: Profile,
    pub limits: Limits,
}

/// The sandbox this machine gives, chosen by [`Sandbox::select`]; there is no
/// other way to make one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// bubblewrap at this path, at least [`MIN_BWRAP`].
    Bwrap(PathBuf),
    /// Landlock, the ruleset written against this ABI.
    Landlock { abi: u8 },
}

impl Sandbox {
    /// bubblewrap when a recent enough one can make its namespaces here,
    /// else Landlock when the kernel has ABI [`MIN_LANDLOCK_ABI`] or later,
    /// else why neither is available. Runs bubblewrap once, with the flags
    /// and seccomp programs a real run uses, since a restricted kernel
    /// refuses it even when it is installed.
    pub async fn select(cancel: &CancellationToken) -> Result<Self, String> {
        #[cfg(all(target_os = "linux", feature = "testing"))]
        if crate::backend::confine::abi_override_for_tests().is_some() {
            return Self::landlock();
        }
        let mut why = Vec::new();
        match Self::bwrap(cancel).await {
            Ok(s) => return Ok(s),
            Err(e) => why.push(e),
        }
        match Self::landlock() {
            Ok(s) => return Ok(s),
            Err(e) => why.push(e),
        }
        Err(format!(
            "no sandbox is available here: {} (see docs/SANDBOX.md)",
            why.join("; ")
        ))
    }

    async fn bwrap(cancel: &CancellationToken) -> Result<Self, String> {
        let program = which("bwrap").ok_or("bubblewrap (bwrap) is not installed")?;
        let version = bwrap_version(&program, cancel)
            .await
            .ok_or("bubblewrap did not report its version")?;
        if version < MIN_BWRAP {
            return Err(format!(
                "bubblewrap {}.{}.{} is older than {}.{}.{}",
                version.0, version.1, version.2, MIN_BWRAP.0, MIN_BWRAP.1, MIN_BWRAP.2
            ));
        }
        let sandbox = Sandbox {
            kind: Kind::Bwrap(program),
        };
        sandbox
            .probe(cancel)
            .await
            .map_err(|e| format!("bubblewrap could not start a sandbox: {e}"))?;
        Ok(sandbox)
    }

    fn landlock() -> Result<Self, String> {
        Ok(Sandbox {
            kind: Kind::Landlock {
                abi: landlock_abi_here()?,
            },
        })
    }

    /// Run `true` in this sandbox: whether it starts here.
    async fn probe(&self, cancel: &CancellationToken) -> Result<(), String> {
        let tmp = tempfile::tempdir().map_err(|e| e.to_string())?;
        let dir = tmp.path().canonicalize().map_err(|e| e.to_string())?;
        let truth = which("true").unwrap_or_else(|| PathBuf::from("/usr/bin/true"));
        let plan = Plan {
            program: truth,
            args: Vec::new(),
            env: Vec::new(),
            cwd: dir.clone(),
            read: Vec::new(),
            exec: Vec::new(),
            write: vec![dir],
            profile: Profile::Denylist,
            limits: Limits::default(),
        };
        let spec = self.command(&plan).map_err(|e| e.to_string())?;
        let out = process::run(&spec, Duration::from_secs(10), cancel)
            .await
            .map_err(|e| e.to_string())?;
        if out.success() {
            Ok(())
        } else {
            Err(first_line(&out.combined()).unwrap_or_else(|| "it failed".into()))
        }
    }

    /// For messages: which sandbox, and what holds signals.
    pub fn describe(&self) -> String {
        match &self.kind {
            Kind::Bwrap(_) => "bubblewrap, with its own namespaces and a seccomp profile".into(),
            Kind::Landlock { abi } if *abi >= SCOPED_LANDLOCK_ABI => {
                format!("Landlock ABI {abi} with its scopes, and a seccomp profile")
            }
            Kind::Landlock { abi } => format!(
                "Landlock ABI {abi}, and a seccomp profile that keeps signals to the process"
            ),
        }
    }

    pub fn is_bwrap(&self) -> bool {
        matches!(self.kind, Kind::Bwrap(_))
    }

    /// Tests only (feature `testing`, which only dev-dependencies enable):
    /// for the rest of this process, [`Sandbox::select`] skips bubblewrap
    /// and picks Landlock at `abi`, so a test on a newer kernel runs under
    /// a real ruleset written against that ABI (at 4 or 5 with seccomp's
    /// signal program in place of the scopes), also where `select` is called
    /// deep inside the code under test. It is process-wide, so tests that use
    /// it belong in a test binary of their own. Refused outside
    /// [`MIN_LANDLOCK_ABI`] to [`MAX_LANDLOCK_ABI`], above the kernel's ABI,
    /// and on a system without Landlock. No product build has this.
    #[cfg(feature = "testing")]
    pub fn use_landlock_abi_for_tests(abi: u8) -> Result<(), String> {
        override_landlock_abi(abi)
    }

    /// Tests only (feature `testing`): Landlock at `abi` for a test that
    /// hands its sandbox to the code under test, with nothing changed for
    /// the rest of the process. Prefer it to
    /// [`Sandbox::use_landlock_abi_for_tests`] wherever a sandbox can be
    /// handed in. Refused outside [`MIN_LANDLOCK_ABI`] to
    /// [`MAX_LANDLOCK_ABI`], above the kernel's ABI, and on a system without
    /// Landlock. No product build has this.
    #[cfg(any(test, feature = "testing"))]
    pub fn landlock_at_abi_for_tests(abi: u8) -> Result<Sandbox, String> {
        if !(MIN_LANDLOCK_ABI..=MAX_LANDLOCK_ABI).contains(&abi) {
            return Err(format!(
                "aispice writes Landlock rulesets for ABI {MIN_LANDLOCK_ABI} to {MAX_LANDLOCK_ABI}, not {abi}"
            ));
        }
        let here = landlock_abi_here()?;
        if here < abi {
            return Err(format!("the kernel's Landlock ABI is {here}, below {abi}"));
        }
        Ok(Sandbox {
            kind: Kind::Landlock { abi },
        })
    }

    /// The ABI the Landlock ruleset is written against, under Landlock.
    pub fn landlock_abi(&self) -> Option<u8> {
        match self.kind {
            Kind::Landlock { abi } => Some(abi),
            Kind::Bwrap(_) => None,
        }
    }

    /// The command that runs `plan` in this sandbox.
    pub fn command(&self, plan: &Plan) -> Result<ProcessSpec, SimError> {
        let program = real(&plan.program);
        let mut spec = match &self.kind {
            Kind::Bwrap(bwrap) => {
                let mut spec = ProcessSpec::new(bwrap, &plan.cwd);
                let confine = Confinement::for_bwrap(plan.limits, plan.profile);
                let programs = confine.seccomp().map_or(0, SeccompPolicy::programs);
                spec.args = bwrap_args(plan, &program, programs);
                spec.confine = Some(confine);
                spec
            }
            Kind::Landlock { abi } => {
                let abi = *abi;
                let scoped = abi >= SCOPED_LANDLOCK_ABI;
                let mut read: Vec<PathBuf> = SYSTEM
                    .iter()
                    .chain(SYSTEM_TRY)
                    .chain(DEVICES)
                    .map(PathBuf::from)
                    .collect();
                // Without a merged /usr, /lib, /bin and the rest are real
                // folders the loader and helpers come from.
                read.extend(
                    SYSTEM_LINKS
                        .iter()
                        .map(PathBuf::from)
                        .filter(|p| p.is_dir() && !p.is_symlink()),
                );
                read.extend(plan.read.iter().cloned());
                let landlock = LandlockPolicy {
                    read,
                    exec: executables(&program, &plan.exec),
                    write: plan.write.clone(),
                    rw: vec![PathBuf::from("/dev/null")],
                    min_abi: abi,
                    scopes: if scoped {
                        Scopes::ALL
                    } else {
                        Scopes::default()
                    },
                };
                let seccomp = SeccompPolicy {
                    profile: plan.profile,
                    signals: plan.profile.signals(scoped),
                };
                let confine = Confinement::builder()
                    .landlock(landlock)
                    .seccomp(seccomp)
                    .limits(plan.limits)
                    .build()
                    .map_err(|e| SimError::Config(e.to_string()))?;
                let mut spec = ProcessSpec::new(&program, &plan.cwd);
                spec.args = plan.args.clone();
                spec.env = plan.env.clone();
                spec.confine = Some(confine);
                spec
            }
        };
        spec.clear_env = true;
        Ok(spec)
    }
}

/// `p` with symlinks resolved, so a rule or bind names the file itself.
fn real(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// What Landlock lets the program execute: itself, the plan's other
/// programs and the loader, each by its real path. This keeps it from
/// starting other programs by path but is not a boundary on its own, since
/// the loader can run any readable binary; seccomp and the file, network
/// and scope rules are what hold.
fn executables(program: &Path, others: &[PathBuf]) -> Vec<PathBuf> {
    let mut v = vec![program.to_path_buf()];
    for p in others.iter().map(|p| real(p)).chain(
        LOADERS
            .iter()
            .filter_map(|l| Path::new(l).canonicalize().ok()),
    ) {
        if !v.contains(&p) {
            v.push(p);
        }
    }
    v
}

/// The bubblewrap arguments that run `plan` (its program at `program`),
/// with `programs` seccomp programs to install.
fn bwrap_args(plan: &Plan, program: &Path, programs: usize) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |items: &[&dyn AsRef<std::ffi::OsStr>]| {
        for i in items {
            a.push(i.as_ref().to_os_string());
        }
    };
    push(&[
        &"--unshare-all",
        &"--unshare-user",
        &"--disable-userns",
        &"--assert-userns-disabled",
        &"--die-with-parent",
        &"--new-session",
        &"--clearenv",
    ]);
    for (k, v) in &plan.env {
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
        &"--proc", &"/proc", &"--dev", &"/dev", &"--size", &size, &"--tmpfs", &"/tmp",
    ]);
    // Read-only, then writable; a path already visible through the system
    // folders or an earlier bind is not bound again.
    let mut shown: Vec<PathBuf> = SYSTEM.iter().map(PathBuf::from).collect();
    let read_only = std::iter::once(program.to_path_buf())
        .chain(plan.exec.iter().map(|p| real(p)))
        .chain(plan.read.iter().cloned());
    for p in read_only {
        if !p.exists() || shown.iter().any(|s| p.starts_with(s)) {
            continue;
        }
        push(&[&"--ro-bind", &p, &p]);
        shown.push(p);
    }
    for w in &plan.write {
        push(&[&"--bind", w, w]);
    }
    push(&[&"--chdir", &plan.cwd]);
    // The seccomp programs arrive as descriptors 3.. (backend::confine
    // moves them there); bubblewrap installs them before the program runs.
    for i in 0..programs {
        push(&[&"--add-seccomp-fd", &(3 + i).to_string()]);
    }
    push(&[&"--", &program]);
    a.extend(plan.args.iter().cloned());
    a
}

/// bubblewrap's version, from `bwrap --version` ("bubblewrap 0.9.0").
async fn bwrap_version(bwrap: &Path, cancel: &CancellationToken) -> Option<(u32, u32, u32)> {
    let mut spec = ProcessSpec::new(bwrap, std::env::temp_dir()).arg("--version");
    spec.clear_env = true;
    let out = process::run(&spec, Duration::from_secs(10), cancel)
        .await
        .ok()?;
    parse_bwrap_version(&out.stdout)
}

fn parse_bwrap_version(text: &str) -> Option<(u32, u32, u32)> {
    let v = text.split_whitespace().nth(1)?;
    let mut parts = v.split('.').map(|p| p.parse::<u32>().ok());
    Some((
        parts.next()??,
        parts.next()??,
        parts.next().flatten().unwrap_or(0),
    ))
}

/// The ABI to write rulesets against on this kernel: its own, capped at
/// [`MAX_LANDLOCK_ABI`], if it is at least [`MIN_LANDLOCK_ABI`].
#[cfg(target_os = "linux")]
fn landlock_abi_here() -> Result<u8, String> {
    landlock_abi_from(crate::backend::confine::abi())
}

#[cfg(not(target_os = "linux"))]
fn landlock_abi_here() -> Result<u8, String> {
    Err("this platform has no Landlock".into())
}

#[cfg(all(target_os = "linux", feature = "testing"))]
fn override_landlock_abi(abi: u8) -> Result<(), String> {
    crate::backend::confine::override_abi_for_tests(abi)
}

#[cfg(all(not(target_os = "linux"), feature = "testing"))]
fn override_landlock_abi(abi: u8) -> Result<(), String> {
    Err(format!("this platform has no Landlock for ABI {abi}"))
}

/// [`landlock_abi_here`] for a given kernel ABI.
#[cfg(any(target_os = "linux", test))]
fn landlock_abi_from(abi: i32) -> Result<u8, String> {
    match abi {
        a if a >= i32::from(MIN_LANDLOCK_ABI) => Ok(a.min(i32::from(MAX_LANDLOCK_ABI)) as u8),
        0 => Err("the kernel has no Landlock support".into()),
        a => Err(format!(
            "the kernel's Landlock ABI {a} is below {MIN_LANDLOCK_ABI}, which restricts truncation and network access"
        )),
    }
}

fn first_line(log: &str) -> Option<String> {
    log.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(200).collect())
}

/// Every sandbox this machine can run, for tests to run each: bubblewrap,
/// Landlock at the kernel's ABI, and on a kernel at ABI 6 or later also
/// Landlock at ABI 4, so the seccomp signal program is exercised there too.
/// With `AISPICE_REQUIRE_SANDBOXES` set (`bwrap`, `landlock`, or both,
/// comma-separated), a missing one fails instead of being skipped: CI sets
/// it where it has made both available.
#[cfg(test)]
pub(crate) async fn every_available() -> Vec<Sandbox> {
    let cancel = CancellationToken::new();
    let mut v = Vec::new();
    let bwrap = Sandbox::bwrap(&cancel).await;
    let landlock = Sandbox::landlock();
    let required = std::env::var("AISPICE_REQUIRE_SANDBOXES").unwrap_or_default();
    for (name, got) in [
        ("bwrap", bwrap.as_ref().err()),
        ("landlock", landlock.as_ref().err()),
    ] {
        if let Some(why) = got {
            assert!(
                !required.split(',').any(|r| r.trim() == name),
                "{name} is required here but unavailable: {why}"
            );
            eprintln!("note: no {name} sandbox here: {why}");
        }
    }
    v.extend(bwrap);
    if let Ok(s) = landlock {
        if s.landlock_abi().is_some_and(|a| a >= SCOPED_LANDLOCK_ABI) {
            v.push(Sandbox {
                kind: Kind::Landlock {
                    abi: MIN_LANDLOCK_ABI,
                },
            });
        }
        v.push(s);
    }
    v
}

#[cfg(test)]
impl Sandbox {
    pub(crate) fn bwrap_for_tests(program: PathBuf) -> Self {
        Sandbox {
            kind: Kind::Bwrap(program),
        }
    }

    #[cfg(unix)]
    pub(crate) fn landlock_for_tests(abi: u8) -> Self {
        Sandbox {
            kind: Kind::Landlock { abi },
        }
    }
}

#[cfg(test)]
mod tests;
