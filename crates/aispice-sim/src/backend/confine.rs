//! Confining a child process before it execs (Linux only): Landlock, a
//! seccomp filter, and resource limits.
//!
//! aispice runs xschem inside bubblewrap where that can start. Where it
//! cannot (an unprivileged-user-namespace restriction without a bwrap
//! AppArmor profile, the default on Ubuntu 24.04), it confines the process
//! itself. xschem evaluates Tcl from the files it opens, so the confinement
//! is strict:
//!
//! - **Landlock** (Linux 6.2 and later, ABI 6 and later, so that abstract
//!   UNIX sockets and signals to other domains can be scoped off) at a
//!   hard requirement: if the kernel cannot provide every access handled
//!   here, the run is refused rather than silently weakened. The process
//!   may read the system, the library and the PDK; execute only xschem and
//!   the few helpers it runs; write only its private output, home and tmp;
//!   and it may make no TCP connection, no abstract-socket connection, and
//!   send no signal outside its own domain. Every rule is built from a
//!   descriptor opened `O_PATH|O_NOFOLLOW`, so a path cannot be swapped for
//!   a symlink between the check and the rule.
//! - **seccomp** denies the system calls a confined netlister never needs
//!   but an escape would use: creating a socket, connecting or binding or
//!   sending on one, tracing or reading another process, the keyring, BPF,
//!   and joining or creating namespaces. Everything else, including
//!   `socketpair` and `execve`, is allowed, and the filter is inherited
//!   across exec so the helpers xschem runs are bound by it too.
//! - **Resource limits** cap the output size and address space, and the
//!   process is asked to die with its parent.
//!
//! The Landlock ruleset and the seccomp program are built here, in the
//! parent, where allocation is safe. The child's `pre_exec` hook does
//! nothing but a few syscalls over already-built state, so it allocates
//! nothing and is safe to run after a fork beside the async runtime's
//! threads.

#![allow(unsafe_code)]

use super::process::{Confinement, Landlock};
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, Ruleset, RulesetAttr,
    RulesetCreatedAttr, Scope,
};
use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
use std::collections::BTreeMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// The lowest Landlock ABI aispice will use. ABI 6 (Linux 6.12 in the crate's
/// table, 6.2 for the base feature; the scope rules used here need ABI 6) is
/// the first that can scope abstract UNIX sockets and signals.
pub const MIN_ABI: i32 = 6;

/// The ABI the ruleset is written against. At a hard requirement the kernel
/// must support all of it, which [`MIN_ABI`] guarantees before this is used.
const ABI_TARGET: ABI = ABI::V6;

/// Landlock's ABI version on this machine, or 0 when the kernel has no
/// Landlock. Read from the kernel with the version probe of
/// `landlock_create_ruleset`, since the crate does not expose a runtime
/// value on purpose.
pub fn abi() -> i32 {
    const VERSION: u32 = 1;
    let r = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            VERSION,
        )
    };
    if r < 0 { 0 } else { r as i32 }
}

/// Open `p` as an `O_PATH|O_NOFOLLOW` descriptor and report whether it is a
/// directory. A symlink at the final component is refused: the caller gives
/// real paths (the private copy it made, and the trusted roots), so a
/// symlink here is something swapped in.
fn open_pinned(p: &Path) -> io::Result<(OwnedFd, bool)> {
    use std::os::fd::FromRawFd;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("a path holds a NUL"))?;
    // SAFETY: a valid C string and flags; the returned fd is owned below.
    let fd = unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a valid fd and a stat buffer we own.
    if unsafe { libc::fstat(owned.as_raw_fd(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let kind = st.st_mode & libc::S_IFMT;
    if kind == libc::S_IFLNK {
        return Err(io::Error::other(format!(
            "{} is a symlink, which the sandbox will not open",
            p.display()
        )));
    }
    Ok((owned, kind == libc::S_IFDIR))
}

/// Add one path to the ruleset with `rights`, opened and pinned by
/// descriptor. Rights that do not apply to a plain file are dropped, and a
/// path that cannot be opened is skipped (nothing to allow).
fn add(
    created: landlock::RulesetCreated,
    p: &Path,
    rights: landlock::BitFlags<AccessFs>,
) -> io::Result<landlock::RulesetCreated> {
    let (fd, is_dir) = match open_pinned(p) {
        Ok(v) => v,
        // A root that is not installed (e.g. an optional /etc file) is not
        // an error; a symlink swap is, and open_pinned already refused it.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(created),
        Err(e) => return Err(e),
    };
    let file_rights =
        AccessFs::Execute | AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate;
    let rights = if is_dir { rights } else { rights & file_rights };
    if rights.is_empty() {
        return Ok(created);
    }
    created
        .add_rule(PathBeneath::new(fd, rights))
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Build the Landlock ruleset for `ll` in this (parent) process and return
/// its descriptor, without enforcing it here.
fn build_landlock(ll: &Landlock) -> io::Result<OwnedFd> {
    let to_io = |e: landlock::RulesetError| io::Error::other(e.to_string());
    // Everything handled, at a hard requirement: anything not granted below
    // is denied, and an older kernel that cannot enforce all of it fails
    // here rather than running weakly. No rule grants TCP, so no bind or
    // connect is possible; the scope cuts off abstract sockets and signals.
    let mut created = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI_TARGET))
        .map_err(to_io)?
        .handle_access(AccessNet::from_all(ABI_TARGET))
        .map_err(to_io)?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)
        .map_err(to_io)?
        .create()
        .map_err(to_io)?;
    // Rights per category; `add` drops the directory-only bits for a plain
    // file, so one read set serves both files and directories.
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    let exec = AccessFs::Execute | AccessFs::ReadFile;
    let write = AccessFs::from_write(ABI_TARGET) | AccessFs::ReadFile | AccessFs::ReadDir;
    let rw = AccessFs::ReadFile | AccessFs::WriteFile;
    for p in &ll.read {
        created = add(created, p, read)?;
    }
    for p in &ll.exec {
        created = add(created, p, exec)?;
    }
    for p in &ll.write {
        created = add(created, p, write)?;
    }
    for p in &ll.rw {
        created = add(created, p, rw)?;
    }
    Option::<OwnedFd>::from(created)
        .ok_or_else(|| io::Error::other("this kernel has no usable Landlock support"))
}

/// Build the seccomp program that denies the syscalls a netlister never
/// needs but an escape would use. Everything else is allowed, so `socketpair`
/// (a local pair that reaches nowhere) and `execve` still work.
fn build_seccomp() -> io::Result<BpfProgram> {
    let denied: &[libc::c_long] = &[
        libc::SYS_socket,
        libc::SYS_connect,
        libc::SYS_bind,
        libc::SYS_sendto,
        libc::SYS_sendmsg,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_bpf,
        libc::SYS_unshare,
        libc::SYS_setns,
    ];
    // c_long is i64 on the 64-bit targets aispice builds for, but the
    // conversion keeps this correct on a 32-bit libc where it is i32.
    #[allow(clippy::useless_conversion)]
    let rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> =
        denied.iter().map(|n| (i64::from(*n), Vec::new())).collect();
    let arch = std::env::consts::ARCH
        .try_into()
        .map_err(|_| io::Error::other("seccomp has no rules for this architecture"))?;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EACCES as u32),
        arch,
    )
    .map_err(|e| io::Error::other(e.to_string()))?;
    BpfProgram::try_from(filter).map_err(|e| io::Error::other(e.to_string()))
}

/// Set one resource limit. A syscall only, safe in `pre_exec`.
unsafe fn setrlimit(resource: libc::__rlimit_resource_t, value: u64) -> io::Result<()> {
    let lim = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: a valid resource id and a stack rlimit.
    if unsafe { libc::setrlimit(resource, &lim) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Confine `cmd`'s child: build the Landlock ruleset and the seccomp program
/// now, and install a `pre_exec` hook that applies the limits, the seccomp
/// filter and the ruleset. The hook makes no allocation and holds no lock.
pub fn apply(cmd: &mut tokio::process::Command, c: &Confinement) -> io::Result<()> {
    let ruleset = match &c.landlock {
        Some(ll) => Some(build_landlock(ll)?),
        None => None,
    };
    let seccomp = match &c.landlock {
        Some(_) => Some(build_seccomp()?),
        None => None,
    };
    let fsize = c.rlimit_fsize;
    let as_limit = c.rlimit_as;
    // SAFETY: the hook runs in the forked child before exec. It only makes
    // syscalls over state built in the parent (the two limit values, the
    // seccomp program and the ruleset descriptor); it allocates nothing and
    // takes no lock, so it cannot deadlock against a thread that held one at
    // fork. The ruleset fd is valid in the child and closes on exec.
    unsafe {
        cmd.pre_exec(move || {
            if let Some(v) = fsize {
                setrlimit(libc::RLIMIT_FSIZE, v)?;
            }
            if let Some(v) = as_limit {
                setrlimit(libc::RLIMIT_AS, v)?;
            }
            // Die if aispice dies, so no confined xschem is left behind.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            // seccompiler sets no_new_privs (which Landlock also needs) and
            // installs the filter, over a borrowed slice, with no allocation.
            if let Some(bpf) = &seccomp {
                seccompiler::apply_filter(bpf)
                    .map_err(|_| io::Error::from_raw_os_error(libc::EPERM))?;
            }
            if let Some(fd) = &ruleset {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, fd.as_raw_fd(), 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seccomp program refuses creating any socket (so no TCP, UDP,
    /// abstract-UNIX or pathname-UNIX connection is possible) while leaving a
    /// local `socketpair` working. Checked in a forked child so the filter
    /// does not bind the test runner; the child reports the outcome by exit
    /// code, having allocated nothing after the fork.
    #[test]
    fn seccomp_denies_sockets_but_allows_socketpair() {
        let bpf = build_seccomp().expect("build the seccomp program");
        // SAFETY: the child only makes syscalls and _exit, no allocation.
        match unsafe { libc::fork() } {
            -1 => panic!("fork failed"),
            0 => {
                let apply_ok = seccompiler::apply_filter(&bpf).is_ok();
                let denied = |r: i32| {
                    r < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EACCES)
                };
                let inet = denied(unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) });
                let unix = denied(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) });
                let udp = denied(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) });
                let mut sv = [0i32; 2];
                let pair_ok = unsafe {
                    libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr())
                } == 0;
                let good = apply_ok && inet && unix && udp && pair_ok;
                unsafe { libc::_exit(if good { 0 } else { 1 }) };
            }
            pid => {
                let mut status = 0;
                unsafe { libc::waitpid(pid, &mut status, 0) };
                let code = libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status));
                assert_eq!(code, Some(0), "seccomp did not deny sockets as expected");
            }
        }
    }
}
